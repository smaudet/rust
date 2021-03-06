// Copyright 2012 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

fn ignore<F>(_f: F) where F: for<'z> FnOnce(&'z int) -> &'z int {}

fn nested() {
    let y = 3;
    ignore(
        |z| { //~ ERROR `y` does not live long enough
            if false { &y } else { z }
        });
}

fn main() {}
