#![feature(async_await)]

mod qrw_lock;

use std::io::{BufRead, Cursor};

use bytes::BytesMut;
use futures::future::TryFutureExt;
use futures::compat::Future01CompatExt;
use rand::Rng;

use crate::qrw_lock::QrwLock;


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
    let mut inner = self.inner.clone().write()
      .compat().compat()  // <----- remove this line and it builds successfully...
    .await.unwrap();
    let b = b.freeze();

    // ... or reverse the order of the next two lines and it builds successfully (even with the
    //     above line left in)
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
