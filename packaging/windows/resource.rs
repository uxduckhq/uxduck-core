//! Both Windows launchers need an executable icon, including portable builds.
pub fn embed(binary: &str) {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }
    let directory =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/windows");
    let icon = directory.join("uxduck.ico");
    let resource = directory.join("uxduck.rc");
    assert!(
        icon.is_file(),
        "Required Windows icon {} is missing. Run node packaging/windows/icon.cjs.",
        icon.display()
    );
    println!("cargo:rerun-if-changed={}", icon.display());
    println!("cargo:rerun-if-changed={}", resource.display());
    if let Err(error) = embed_resource::compile_for(
        resource,
        [binary],
        embed_resource::ParamsIncludeDirs([directory]),
    )
    .manifest_required()
    {
        panic!(
            "Could not compile UXDuck's Windows icon: {error:?}. Install the Windows SDK resource compiler."
        );
    }
}
