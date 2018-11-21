use super::{Device, RawDevice, Surface, DeviceHandler, DevPath};

use drm::Device as BasicDevice;
use drm::control::{crtc, connector, encoder, Device as ControlDevice, Mode, ResourceInfo};
use nix::libc::dev_t;
use nix::sys::stat::fstat;

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::{Rc, Weak};
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, RwLock};
use std::sync::atomic::{AtomicBool, Ordering};

mod surface;
pub use self::surface::LegacyDrmSurface;
use self::surface::State;

pub mod error;
use self::error::*;

#[cfg(feature = "backend_session")]
pub mod session;

pub struct LegacyDrmDevice<A: AsRawFd + 'static> {
    dev: Rc<Dev<A>>,
    dev_id: dev_t,
    priviledged: bool,
    active: Arc<AtomicBool>,
    old_state: HashMap<crtc::Handle, (crtc::Info, Vec<connector::Handle>)>,
    backends: Rc<RefCell<HashMap<crtc::Handle, Weak<LegacyDrmSurface<A>>>>>,
    handler: Option<RefCell<Box<DeviceHandler<Device=LegacyDrmDevice<A>>>>>,
    logger: ::slog::Logger,
}

pub(in crate::backend::drm) struct Dev<A: AsRawFd + 'static>(A);
impl<A: AsRawFd + 'static> AsRawFd for Dev<A> {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}
impl<A: AsRawFd + 'static> BasicDevice for Dev<A> {}
impl<A: AsRawFd + 'static> ControlDevice for Dev<A> {}

impl<A: AsRawFd + 'static> LegacyDrmDevice<A> {
    /// Create a new `LegacyDrmDevice` from an open drm node
    ///
    /// Returns an error if the file is no valid drm node or context creation was not
    /// successful.
    pub fn new<L>(dev: A, logger: L) -> Result<Self>
    where
        L: Into<Option<::slog::Logger>>,
    {
        let log = ::slog_or_stdlog(logger).new(o!("smithay_module" => "backend_drm"));

        let dev_id = fstat(dev.as_raw_fd())
            .chain_err(|| ErrorKind::UnableToGetDeviceId)?
            .st_rdev;

        let mut drm = LegacyDrmDevice {
            // Open the drm device and create a context based on that
            dev: Rc::new(Dev(dev)),
            dev_id,
            priviledged: true,
            active: Arc::new(AtomicBool::new(true)),
            old_state: HashMap::new(),
            backends: Rc::new(RefCell::new(HashMap::new())),
            handler: None,
            logger: log.clone(),
        };

        info!(log, "DrmDevice initializing");

        // we want to modeset, so we better be the master, if we run via a tty session
        if drm.set_master().is_err() {
            warn!(log, "Unable to become drm master, assuming unpriviledged mode");
            drm.priviledged = false;
        };

        let res_handles = drm.resource_handles().chain_err(|| {
            ErrorKind::DrmDev(format!("Error loading drm resources on {:?}", drm.dev_path()))
        })?;
        for &con in res_handles.connectors() {
            let con_info = connector::Info::load_from_device(&drm, con).chain_err(|| {
                ErrorKind::DrmDev(format!("Error loading connector info on {:?}", drm.dev_path()))
            })?;
            if let Some(enc) = con_info.current_encoder() {
                let enc_info = encoder::Info::load_from_device(&drm, enc).chain_err(|| {
                    ErrorKind::DrmDev(format!("Error loading encoder info on {:?}", drm.dev_path()))
                })?;
                if let Some(crtc) = enc_info.current_crtc() {
                    let info = crtc::Info::load_from_device(&drm, crtc).chain_err(|| {
                        ErrorKind::DrmDev(format!("Error loading crtc info on {:?}", drm.dev_path()))
                    })?;
                    drm.old_state
                        .entry(crtc)
                        .or_insert((info, Vec::new()))
                        .1
                        .push(con);
                }
            }
        }

        Ok(drm)
    }

    pub fn dev_id(&self) -> dev_t {
        self.dev_id
    }
}

impl<A: AsRawFd + 'static> AsRawFd for LegacyDrmDevice<A> {
    fn as_raw_fd(&self) -> RawFd {
        self.dev.0.as_raw_fd()
    }
}

impl<A: AsRawFd + 'static> BasicDevice for LegacyDrmDevice<A> {}
impl<A: AsRawFd + 'static> ControlDevice for LegacyDrmDevice<A> {}

impl<A: AsRawFd + 'static> Device for LegacyDrmDevice<A> {
    type Surface = LegacyDrmSurface<A>;
    type Return = Rc<LegacyDrmSurface<A>>;

    fn set_handler(&mut self, handler: impl DeviceHandler<Device=Self> + 'static) {
        self.handler = Some(RefCell::new(Box::new(handler)));
    }
    
    fn clear_handler(&mut self) {
        let _ = self.handler.take();
    }

    fn create_surface(
        &mut self,
        crtc: crtc::Handle,
        mode: Mode,
        connectors: impl Into<<Self::Surface as Surface>::Connectors>
    ) -> Result<Rc<LegacyDrmSurface<A>>> {
        if self.backends.borrow().contains_key(&crtc) {
            bail!(ErrorKind::CrtcAlreadyInUse(crtc));
        }

        if !self.active.load(Ordering::SeqCst) {
            bail!(ErrorKind::DeviceInactive);
        }

        let connectors: HashSet<_> = connectors.into();
        // check if we have an encoder for every connector and the mode mode
        for connector in &connectors {
            let con_info = connector::Info::load_from_device(self, *connector).chain_err(|| {
                ErrorKind::DrmDev(format!("Error loading connector info on {:?}", self.dev_path()))
            })?;

            // check the mode
            if !con_info.modes().contains(&mode) {
                bail!(ErrorKind::ModeNotSuitable(mode));
            }

            // check for every connector which encoders it does support
            let encoders = con_info
                .encoders()
                .iter()
                .map(|encoder| {
                    encoder::Info::load_from_device(self, *encoder).chain_err(|| {
                        ErrorKind::DrmDev(format!("Error loading encoder info on {:?}", self.dev_path()))
                    })
                }).collect::<Result<Vec<encoder::Info>>>()?;

            // and if any encoder supports the selected crtc
            let resource_handles = self.resource_handles().chain_err(|| {
                ErrorKind::DrmDev(format!("Error loading drm resources on {:?}", self.dev_path()))
            })?;
            if !encoders
                .iter()
                .map(|encoder| encoder.possible_crtcs())
                .any(|crtc_list| resource_handles.filter_crtcs(crtc_list).contains(&crtc))
            {
                bail!(ErrorKind::NoSuitableEncoder(con_info, crtc))
            }
        }

        // configuration is valid, the kernel will figure out the rest
        let logger = self.logger.new(o!("crtc" => format!("{:?}", crtc)));
        
        let state = State {
            mode,
            connectors,
        };

        let backend = Rc::new(LegacyDrmSurface {
            dev: self.dev.clone(),
            crtc,
            state: RwLock::new(state.clone()),
            pending: RwLock::new(state),
            logger,
        });
        
        self.backends.borrow_mut().insert(crtc, Rc::downgrade(&backend));
        Ok(backend)
    }
    
    fn process_events(&mut self) {
        match crtc::receive_events(self) {
            Ok(events) => for event in events {
                if let crtc::Event::PageFlip(event) = event {
                    if self.active.load(Ordering::SeqCst) {
                        if let Some(backend) = self.backends.borrow().get(&event.crtc).iter().flat_map(|x| x.upgrade()).next() {
                            trace!(self.logger, "Handling event for backend {:?}", event.crtc);
                            if let Some(handler) = self.handler.as_ref() {
                                handler.borrow_mut().vblank(&backend);
                            }
                        } else {
                            self.backends.borrow_mut().remove(&event.crtc);
                        }
                    }
                }
            },
            Err(err) => if let Some(handler) = self.handler.as_ref() {
                handler.borrow_mut().error(ResultExt::<()>::chain_err(Err(err), || 
                    ErrorKind::DrmDev(format!("Error processing drm events on {:?}", self.dev_path()))
                ).unwrap_err());
            }
        }
    }
}

impl<A: AsRawFd + 'static> RawDevice for LegacyDrmDevice<A> {
    type Surface = LegacyDrmSurface<A>;
}

impl<A: AsRawFd + 'static> Drop for LegacyDrmDevice<A> {
    fn drop(&mut self) {
        self.backends.borrow_mut().clear();
        if Rc::strong_count(&self.dev) > 1 {
            panic!("Pending DrmBackends. You need to free all backends before the DrmDevice gets destroyed");
        }
        if self.active.load(Ordering::SeqCst) {
            for (handle, (info, connectors)) in self.old_state.drain() {
                if let Err(err) = crtc::set(
                    &*self.dev,
                    handle,
                    info.fb(),
                    &connectors,
                    info.position(),
                    info.mode(),
                ) {
                    error!(self.logger, "Failed to reset crtc ({:?}). Error: {}", handle, err);
                }
            }
            if self.priviledged {
                if let Err(err) = self.drop_master() {
                    error!(self.logger, "Failed to drop drm master state. Error: {}", err);
                }
            }
        }
    }
}
