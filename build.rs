//! Sous Windows : intègre l'icône et les informations de l'éditeur dans le .exe
//! (visibles dans l'Explorateur, la barre des tâches et « Applications installées »).

fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    #[cfg(windows)]
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico")
            .set("ProductName", "one-mouse")
            .set("FileDescription", "one-mouse")
            .set("CompanyName", "BloopStudio")
            .set("LegalCopyright", "Copyright © BloopStudio");
        if let Err(e) = res.compile() {
            println!("cargo:warning=icône Windows non intégrée : {e}");
        }
    }
}
