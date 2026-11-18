//! MonsGeek RY5088 の読み取り専用診断 (書き込みはしない)。
//! `cargo run -p devhub-hid --example mgprobe`
//!
//! VID 3151 の設定 IF (usage FFFF/02) を開き、GET_USB_VERSION の device id・ファーム版
//! (精度係数)・ポーリング・LED・KB オプションの生応答と、先頭数キーのトラベル設定を表示する。

use devhub_core::model::*;
use devhub_hid::{monsgeek, transport};

fn main() {
    let api = transport::open().expect("hidapi");
    let Some(raw) = api
        .device_list()
        .find(|d| d.vendor_id() == 0x3151 && d.usage_page() == 0xFFFF && d.usage() == 0x02)
        .cloned()
    else {
        eprintln!("MonsGeek 設定 IF (3151 FFFF/02) が見つかりません");
        std::process::exit(1);
    };
    let pid = raw.product_id();
    let info = DeviceInfo {
        id: DeviceId(format!("mgprobe:{pid:04x}")),
        kind: DeviceKind::Keyboard,
        name: raw.product_string().unwrap_or("MonsGeek").into(),
        vendor: "MonsGeek".into(),
        model: "ry5088".into(),
        vid: 0x3151,
        pid,
        transport: Transport::Wired,
        firmware: None,
        dongle_firmware: None,
        serial: None,
        driver: DriverKind::Monsgeek,
        supported: true,
        asleep: false,
        battery: None,
        board_id: None,
    };
    let dev = raw.open_device(&api).expect("open");
    let d = monsgeek::open(dev, info).expect("driver");
    println!("== 3151:{pid:04x}");
    print!("{}", monsgeek::probe(&d));
}
