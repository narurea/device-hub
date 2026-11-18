//! Logitech HID++ 2.0 の読み取り専用診断 (書き込みはしない)。
//! `cargo run -p devhub-hid --example lgprobe`
//!
//! 有線 (046d:c09b) → SL2 受信機 (c54d) → 汎用 Lightspeed 受信機 (c547) の順で
//! HID++ long コレクション (FF00/0002) を探し、機能一覧・オンボードメモリ・
//! アクティブプロファイルのボタン 4 バイト・レポートレート・電池を表示する。
//! 実機検証で docs/PROTOCOL_NOTES.md §6 の「要実機確認」を埋めるのに使う。

use devhub_core::model::*;
use devhub_core::registry;
use devhub_hid::{logitech, transport};

fn main() {
    let api = transport::open().expect("hidapi");
    let found = [0xC09Bu16, 0xC54D, 0xC547].iter().find_map(|&pid| {
        api.device_list()
            .find(|d| {
                d.vendor_id() == 0x046D
                    && d.product_id() == pid
                    && d.usage_page() == 0xFF00
                    && d.usage() == 0x02
            })
            .map(|d| (pid, d.clone()))
    });
    let Some((pid, raw)) = found else {
        eprintln!(
            "Logitech HID++ long collection (046d:c09b/c54d/c547 FF00/0002) が見つかりません"
        );
        std::process::exit(1);
    };
    let spec = registry::KNOWN
        .iter()
        .find(|k| k.vid == 0x046D && k.pid == pid)
        .expect("registry");
    let info = DeviceInfo {
        id: DeviceId(format!("lgprobe:{pid:04x}")),
        kind: DeviceKind::Mouse,
        name: spec.name.into(),
        vendor: "Logitech".into(),
        model: spec.model.into(),
        vid: 0x046D,
        pid,
        transport: spec.transport,
        firmware: None,
        dongle_firmware: None,
        serial: None,
        driver: DriverKind::Logitech,
        supported: true,
        asleep: false,
        battery: None,
        board_id: None,
    };
    let dev = raw.open_device(&api).expect("open");
    let d = logitech::open(dev, info, None).expect("driver");
    println!("== {:04x}:{pid:04x} ({})", 0x046D, spec.name);
    print!("{}", logitech::probe(&d));
}
