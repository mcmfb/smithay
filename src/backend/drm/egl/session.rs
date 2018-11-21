use std::os::unix::io::RawFd;

use backend::egl::native::{Backend, NativeDisplay, NativeSurface};
use backend::session::{AsSessionObserver, SessionObserver};
use backend::drm::Device;
use super::{EglDevice};

/// `SessionObserver` linked to the `DrmDevice` it was created from.
pub struct EglDeviceObserver<S: SessionObserver + 'static> {
    observer: S,
}

impl<
    S: SessionObserver + 'static,
    B: Backend<Surface=<D as Device>::Surface> + 'static,
    D: Device + NativeDisplay<B> + AsSessionObserver<S> + 'static,
> AsSessionObserver<EglDeviceObserver<S>> for EglDevice<B, D>
    where <D as Device>::Surface: NativeSurface
{
    fn observer(&mut self) -> EglDeviceObserver<S> {
        EglDeviceObserver {
            observer: (**self.dev.borrow_mut()).observer(),
        }
    }
}

impl<S: SessionObserver + 'static> SessionObserver for EglDeviceObserver<S> {
    fn pause(&mut self, devnum: Option<(u32, u32)>) {
        self.observer.pause(devnum);
    }

    fn activate(&mut self, devnum: Option<(u32, u32, Option<RawFd>)>) {
        self.observer.activate(devnum);
    }
}
