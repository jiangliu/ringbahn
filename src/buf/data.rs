use std::any::Any;

use crate::Cancellation;

#[derive(Default)]
pub struct Data {
    inner: Option<Inner>
}

impl Data {
    pub fn alloc_bytes(&mut self, len: usize) -> &mut [u8] {
        if self.inner.is_none() {
            self.inner = Some(Inner::Buffer(vec![0; len].into_boxed_slice()));
        }
        self.inner.as_mut().unwrap().bytes_mut().unwrap()
    }

    pub fn alloc<T: Send + Sync + 'static>(&mut self, data: T) -> &mut T  {
        if self.inner.is_none() {
            self.inner = Some(Inner::Object(Box::new(data)));
        }
        self.inner.as_mut().unwrap().downcast().unwrap()
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        self.inner.as_ref().and_then(|inner| inner.bytes())
    }

    pub fn cancellation(&mut self) -> Cancellation {
        self.inner.take().map_or_else(Cancellation::null, Inner::cancellation)
    }
}

enum Inner {
    Buffer(Box<[u8]>),
    Object(Box<dyn Any + Send + Sync>),
}

impl Inner {
    fn downcast<T: Any + Send + Sync>(&mut self) -> Option<&mut T> {
        match self {
            Inner::Object(object)   => object.downcast_mut(),
            Inner::Buffer(_)        => None,
        }
    }

    fn bytes_mut(&mut self) -> Option<&mut [u8]> {
        match self {
            Inner::Buffer(bytes)    => Some(&mut bytes[..]),
            Inner::Object(_)        => None,
        }
    }

    fn bytes(&self) -> Option<&[u8]> {
        match self {
            Inner::Buffer(bytes)    => Some(&bytes[..]),
            Inner::Object(_)        => None,
        }
    }

    fn cancellation(self) -> Cancellation {
        match self {
            Inner::Buffer(bytes)    => Cancellation::buffer(bytes),
            Inner::Object(object)   => Cancellation::dyn_object(object),
        }
    }
}