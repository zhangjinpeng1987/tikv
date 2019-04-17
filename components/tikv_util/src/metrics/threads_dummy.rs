// Copyright 2018 TiKV Project Authors. Licensed under Apache-2.0.

/*!

Currently we does not support collecting CPU usage of threads for systems
other than Linux. PRs are welcome!

*/

use std::io;

pub fn monitor_threads<S: Into<String>>(_: S) -> io::Result<()> {
    Ok(())
}
