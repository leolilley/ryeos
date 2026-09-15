include!(concat!(env!("OUT_DIR"), "/generated.rs"));
unsafe extern "C" {
    fn native_answer() -> i32;
    fn pthread_atfork(
        prepare: Option<unsafe extern "C" fn()>,
        parent: Option<unsafe extern "C" fn()>,
        child: Option<unsafe extern "C" fn()>,
    ) -> i32;
}
fn main() {
    assert_eq!(macro_probe::identity!(unsafe { native_answer() }), EXPECTED);
    // Linkage-only fixture: exercise the pinned nonshared archive, without
    // implementing an OS primitive or requiring an actual child process.
    assert_eq!(unsafe { pthread_atfork(None, None, None) }, 0);
}
#[test]
fn native_and_macro() {
    main();
}
