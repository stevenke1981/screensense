//! (to be implemented) X11 backend.
pub fn create() -> Box<dyn crate::Platform> {
    Box::new(crate::NullPlatform::new("x11 backend not built yet"))
}
