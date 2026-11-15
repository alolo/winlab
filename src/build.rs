fn main() {
    println!("cargo:rerun-if-changed=assets/TaSQ.ico");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winres::WindowsResource::new();

        res.set_icon("assets/TaSQ.ico");

        res.set("FileDescription", "Theatre Show Cue");
        res.set("ProductName", "Theatre Show Cue");
        res.set("CompanyName", "Alo Production");
        res.set("OriginalFilename", "TaSQ.exe");

        res.compile().expect("Failed to compile Windows resources");
    }
}
