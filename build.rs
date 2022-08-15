const TEST_EXPR: &str = "{
#[cfg(unix)]
fn _test<I: std::os::unix::io::AsFd>(_: I) {}
#[cfg(unix)]
fn _test2(_: std::os::unix::io::OwnedFd) {}
#[cfg(windows)]
fn _test<I: std::os::windows::io::AsSocket>(_: I) {}
#[cfg(windows)]
fn _test2(_: std::os::windows::io::OwnedSocket) {}
}";

fn main() {
    let cfg = autocfg::new();

    if cfg.probe_expression(TEST_EXPR) {
        autocfg::emit("has_io_safety");
    }
}
