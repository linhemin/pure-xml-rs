mod common;
use common::*;
use std::time::Duration;
#[test]
fn hang_detected() {
    assert!(matches!(
        run_guarded("h", Duration::from_millis(300), || loop {
            std::hint::spin_loop()
        }),
        Guarded::<()>::TimedOut
    ));
}
#[test]
fn peak_measured() {
    match run_guarded("m", Duration::from_secs(2), || {
        let v = vec![1u8; 100 << 20];
        v.len()
    }) {
        Guarded::Done { peak_bytes, .. } => assert!(peak_bytes >= 100 << 20, "{peak_bytes}"),
        g => panic!("{g:?}"),
    }
}
#[test]
fn panic_caught() {
    assert!(matches!(
        run_guarded("p", Duration::from_secs(2), || -> () { panic!("x") }),
        Guarded::Panicked(_)
    ));
}
