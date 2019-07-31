#![feature(async_await)]

use std::io::{BufRead, Cursor};

use bytes::BytesMut;
use futures::compat::Future01CompatExt;
use qutex::QrwLock;
use rand::Rng;


struct FooInner {
  state: u8,
}

impl FooInner {
  async fn go(&mut self, new_state: u8) -> Result<(), ()> {
    self.state = new_state;
    Ok(())
  }
}

struct Foo {
  inner: QrwLock<FooInner>,
}

impl Foo {
  async fn go(&self, b: BytesMut) -> Result<(), ()> {
    let mut inner = self.inner.clone().write().compat().await.unwrap();
    let b = b.freeze();

    // Reverse the order of the next two lines and it builds successfully.
    let mut c = Cursor::new(b);
    let mut v = Vec::new();

    c.read_until(0x2c, &mut v).unwrap();
   
    inner.go(v[0]).await.unwrap();
    Ok(())
  }
}

#[tokio::main]
pub async fn main() {
  let foo = Foo { inner: QrwLock::new(FooInner { state: 0 }) };
  let mut b = BytesMut::new();
  rand::thread_rng().fill(b.as_mut());
  foo.go(b).await.unwrap();
}
