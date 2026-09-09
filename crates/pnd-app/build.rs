//! 把 `assets/icon.ico` 编进 exe 的资源段。
//!
//! **资源 ID 必须是 1,这一条不能改。** gpui 0.2 的 `load_icon()` 里写死了
//! `LoadImageW(module, PCWSTR(1 as _), IMAGE_ICON, ..)` —— `PCWSTR(1)` 就是
//! `MAKEINTRESOURCE(1)`,取到的 `HICON` 直接当 `WNDCLASSW::hIcon` 注册窗口类。
//! 也就是说只要图标落在 id 1 上,窗口左上角和任务栏的图标都是白送的,业务代码
//! 一行都不用改。换成别的 id,gpui 那次 `LoadImageW` 就会落空,两处一起退回系统
//! 默认图标 —— 所以这个 1 是对着上游写死的常量对齐的,不是随手挑的数字。

fn main() {
    // 只有目标平台是 Windows 才有资源段可言。别的目标(比如顺手 `cargo check`
    // 一个非 Windows 三元组)直接放行:rc.exe 那一步在那儿必然是错的。
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows") {
        return;
    }

    // 下面这张单子就是资源内容的全部来源。注意:build.rs 只要报了一条
    // rerun-if-*,cargo 就只认这张单子、不再盯整个包 —— 所以漏掉哪一项,改了它
    // 之后 exe 里留的就还是旧值。
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_VERSION");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_DESCRIPTION");
    println!("cargo:rerun-if-env-changed=CARGO_PKG_AUTHORS");

    let mut res = winresource::WindowsResource::new();

    // `set_icon` 的默认 name id 正好是 "1",即上面说的那个 1。
    res.set_icon("assets/icon.ico");

    // winresource 自己只填 FileVersion / ProductVersion,外加拿包名充当
    // ProductName 和 FileDescription。剩下几条它不管,这里补上 —— 一律读
    // CARGO_PKG_*,作者名写死在这儿的话,改 Cargo.toml 就会两边对不上。
    let authors = std::env::var("CARGO_PKG_AUTHORS").unwrap_or_default();
    let description = std::env::var("CARGO_PKG_DESCRIPTION").unwrap_or_default();

    // Cargo 里没有"品牌名"这一栏,只有这一条是字面量。
    res.set("ProductName", "Exile Ledger");
    // 任务管理器把 FileDescription 当程序名显示,所以这里要放人话而不是包名。
    res.set("FileDescription", &description);
    res.set("CompanyName", &authors);
    // 不盖年份:build.rs 拿不到一个不会过期的年份,写死了迟早变成陈年谎话。
    res.set("LegalCopyright", &format!("Copyright (C) {authors}"));
    // 这个包只有一个 bin(`exile-ledger`),所以文件名写死是真话(兄弟项目有
    // 两个 bin,那边就只能空着)。任务管理器和"属性"面板会拿它来认这个 exe。
    res.set("InternalName", "exile-ledger.exe");
    res.set("OriginalFilename", "exile-ledger.exe");

    // 缺 rc.exe(没装 Windows SDK)不该把别人的构建拦死在这儿:图标是锦上添花,
    // 没有它 exe 照样能跑,只是退回系统默认图标。所以只警告,不 panic。
    if let Err(err) = res.compile() {
        println!(
            "cargo:warning=图标资源没能编进 exe({err});程序照常运行,只是窗口和任务栏会用系统默认图标。装上 Windows SDK 的 rc.exe 后重新构建即可。"
        );
    }
}
