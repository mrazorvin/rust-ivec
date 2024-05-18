// could be implmented by any resource that needed end frame disposing
pub trait FrameDisposable {
    unsafe fn dispose(&self);
}

#[repr(transparent)]
pub struct DisposeItem {
    pub data: *const dyn FrameDisposable,
}

unsafe impl Sync for DisposeItem {}
