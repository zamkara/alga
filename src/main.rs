use libadwaita::prelude::*;
use libadwaita::{
    ActionRow, Application, ApplicationWindow, HeaderBar, PreferencesGroup, ToastOverlay,
};
use gtk::{
    Box, Button, CheckButton, Image, Label, Orientation,
    ProgressBar, ScrolledWindow, Stack, StackTransitionType, Switch, TextView,
};
use std::cell::RefCell;
use std::env;
use std::process::Stdio;
use std::rc::Rc;
use std::os::unix::process::CommandExt;
use glib::clone;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::oneshot;

fn log_to_desktop(msg: &str) {
    if let Ok(home) = std::env::var("HOME") {
        let desktop_dir = std::path::PathBuf::from(&home).join("Desktop");
        let _ = std::fs::create_dir_all(&desktop_dir);
        let desktop_log = desktop_dir.join("log.txt");
        if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&desktop_log) {
            use std::io::Write;
            let _ = writeln!(file, "{}", msg);
        }
    }
}

#[allow(dead_code)]
mod nm {
    use gtk::gio;
    use glib::ToVariant;

    pub fn system_bus() -> gio::DBusConnection {
        gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>)
            .expect("failed to connect to system bus")
    }

    pub fn call(conn: &gio::DBusConnection, bus: &str, path: &str, iface: &str, method: &str, args: Option<&glib::Variant>) -> Result<glib::Variant, glib::Error> {
        conn.call_sync(Some(bus), path, iface, method, args, None::<&glib::VariantTy>, gio::DBusCallFlags::NONE, -1, None::<&gio::Cancellable>)
    }

    pub fn prop_get(conn: &gio::DBusConnection, bus: &str, path: &str, iface: &str, prop: &str) -> glib::Variant {
        let v = call(conn, bus, path, "org.freedesktop.DBus.Properties", "Get",
            Some(&(iface, prop).to_variant())).expect("Properties.Get failed");
        v.child_value(0).as_variant().expect("unexpected variant type")
    }

    /// Returns `true` if NM reports global connectivity (State == 70).
    pub fn is_online() -> bool {
        let conn = match gio::bus_get_sync(gio::BusType::System, None::<&gio::Cancellable>) {
            Ok(c) => c,
            Err(_) => return false,
        };
        let v = match call(
            &conn,
            "org.freedesktop.NetworkManager",
            "/org/freedesktop/NetworkManager",
            "org.freedesktop.DBus.Properties",
            "Get",
            Some(&("org.freedesktop.NetworkManager", "State").to_variant()),
        ) {
            Ok(v) => v,
            Err(_) => return false,
        };
        // `v` is the (v) tuple from Get. Unwrap: child_value(0) → variant,
        // then as_variant() → the inner u32 variant.
        let state: u32 = v
            .child_value(0)
            .as_variant()
            .and_then(|v| v.get::<u32>())
            .unwrap_or(0);
        state == 70
    }
}

const ALGA_VERSION: &str = env!("CARGO_PKG_VERSION");

fn check_alga_update() -> Result<Option<String>, String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .user_agent("alga/1.0")
            .build()
            .map_err(|e| format!("Client error: {}", e))?;

        let resp = client
            .get("https://api.github.com/repos/zamkara/alga/releases/latest")
            .send()
            .await
            .map_err(|e| format!("Network error: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("GitHub API returned {}", resp.status()));
        }

        let text = resp.text().await.map_err(|e| format!("Read error: {}", e))?;
        let tag = text.split("\"tag_name\":\"")
            .nth(1)
            .and_then(|s| s.split('\"').next())
            .ok_or("Could not parse tag_name")?;
        let tag_clean = tag.trim_start_matches('v');

        if tag_clean == ALGA_VERSION {
            return Ok(None);
        }

        match (semver::Version::parse(ALGA_VERSION), semver::Version::parse(tag_clean)) {
            (Ok(current), Ok(remote)) if remote > current => Ok(Some(tag.to_string())),
            _ => Ok(None),
        }
    })
}

fn download_alga_update(version: &str) -> Result<(), String> {
    let rt = tokio::runtime::Runtime::new().map_err(|e| e.to_string())?;
    rt.block_on(async {
        let client = reqwest::Client::builder()
            .user_agent("alga/1.0")
            .build()
            .map_err(|e| format!("Client error: {}", e))?;

        let url = format!(
            "https://github.com/zamkara/alga/releases/download/{}/alga-x86_64.tar.gz",
            version
        );
        let resp = client.get(&url).send().await.map_err(|e| format!("Download error: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("Download returned {}", resp.status()));
        }

        let bytes = resp.bytes().await.map_err(|e| format!("Read error: {}", e))?;

        let bin_dir = std::path::PathBuf::from("/var/lib/alga/bin");
        let meta_dir = std::path::PathBuf::from("/var/lib/alga");
        std::fs::create_dir_all(&bin_dir).map_err(|e| format!("Create dir error: {}", e))?;

        let tar_path = bin_dir.join("alga.tar.gz");
        std::fs::write(&tar_path, &bytes).map_err(|e| format!("Write error: {}", e))?;

        let output = std::process::Command::new("tar")
            .args(["-xzf", tar_path.to_str().unwrap(), "-C", bin_dir.to_str().unwrap()])
            .output()
            .map_err(|e| format!("tar error: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let _ = std::fs::remove_file(&tar_path);
            return Err(format!("tar extract failed: {}", stderr));
        }

        let _ = std::fs::remove_file(&tar_path);

        let final_path = bin_dir.join("alga");
        let tmp_path = bin_dir.join("alga");
        if !tmp_path.exists() {
            return Err("Extracted binary not found".to_string());
        }

        std::fs::rename(&tmp_path, &final_path).map_err(|e| format!("Rename error: {}", e))?;

        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&final_path, std::fs::Permissions::from_mode(0o755))
            .map_err(|e| format!("chmod error: {}", e))?;

        let metadata = format!("{{\"version\":\"{}\",\"updated_at\":\"{}\"}}", version, chrono_now());
        let _ = std::fs::write(meta_dir.join("current"), &metadata);

        Ok(())
    })
}

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    format!("{}", d.as_secs())
}

fn restart_alga() -> ! {
    let updated = "/var/lib/alga/bin/alga";
    let original = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("/usr/bin/alga"));
    let args: Vec<String> = std::env::args().collect();
    let target = if std::path::Path::new(updated).exists() {
        updated
    } else {
        original.to_str().unwrap_or("/usr/bin/alga")
    };
    let err = std::process::Command::new(target)
        .args(&args[1..])
        .exec();
    panic!("exec failed: {}", err);
}

const BLS_SYNC_SCRIPT: &str = r#"
set -euo pipefail
SYSROOT="${SYSROOT:-/sysroot}"
OSTREE_REPO="$SYSROOT/ostree/repo"
DEPLOY_BASE="$SYSROOT/ostree/deploy/default/deploy"

[ ! -d "$OSTREE_REPO" ] && exit 0
[ ! -d "$DEPLOY_BASE" ] && exit 0

# Cari EFI System Partition — systemd-boot cuma baca dari sini
ESP=""
for candidate in "/boot" "/efi" "/boot/efi"; do
    if mountpoint -q "$candidate" 2>/dev/null && df -T "$candidate" 2>/dev/null | grep -q vfat; then
        ESP="$candidate"
        break
    fi
done
if [ -z "$ESP" ]; then
    ESP_DEV=""
    if command -v blkid >/dev/null 2>&1 && blkid -L EFI-SYSTEM >/dev/null 2>&1; then
        ESP_DEV=$(blkid -L EFI-SYSTEM 2>/dev/null)
    fi
    if [ -z "$ESP_DEV" ] && command -v lsblk >/dev/null 2>&1; then
        ESP_DEV=$(lsblk -o NAME,FSTYPE,LABEL -rn 2>/dev/null | awk '$2 == "vfat" && $3 == "EFI-SYSTEM" {print "/dev/"$1}' | head -1)
    fi
    if [ -n "$ESP_DEV" ]; then
        ESP="/mnt/esp"
        mkdir -p "$ESP" 2>/dev/null
        mount "$ESP_DEV" "$ESP" 2>/dev/null || ESP=""
    fi
fi
[ -z "$ESP" ] && exit 0

if ! touch "$SYSROOT/.ark-bls-check" 2>/dev/null; then
    mount -o remount,rw "$SYSROOT" 2>/dev/null || true
fi
rm -f "$SYSROOT/.ark-bls-check" 2>/dev/null || true

deployments=$(ostree admin --sysroot="$SYSROOT" status 2>/dev/null | grep -oP 'ostree/deploy/default/deploy/\K[^ ]+' || true)
if [ -z "$deployments" ]; then
    deployments=$(ls -d "$DEPLOY_BASE"/*/ 2>/dev/null | xargs -n1 basename 2>/dev/null || true)
fi
[ -z "$deployments" ] && exit 0

mkdir -p "$ESP/loader/entries" "$ESP/ostree"
ROOT_UUID=$(findmnt -n -o UUID "$SYSROOT" 2>/dev/null || echo "")

count=0
for deploy_id in $deployments; do
    deploy_id=$(echo "$deploy_id" | tr -d '\n\r ')
    [ -z "$deploy_id" ] && continue
    count=$((count + 1))
    deploy_path="$DEPLOY_BASE/$deploy_id"
    modules_dir="$deploy_path/usr/lib/modules"
    [ ! -d "$modules_dir" ] && continue
    kver=$(ls "$modules_dir" 2>/dev/null | grep -v 'extramodules' | head -1)
    [ -z "$kver" ] && continue
    [ ! -f "$modules_dir/$kver/vmlinuz" ] && continue

    vmlinuz_src="$modules_dir/$kver/vmlinuz"
    vmlinuz_dst="$ESP/ostree/$deploy_id/vmlinuz-$kver"
    initramfs_dst="$ESP/ostree/$deploy_id/initramfs-$kver.img"

    mkdir -p "$ESP/ostree/$deploy_id"
    cp -f "$vmlinuz_src" "$vmlinuz_dst"

    initramfs_src=""
    for candidate in \
        "$modules_dir/$kver/initramfs.img" \
        "$deploy_path/boot/initramfs-$kver.img" \
        "$deploy_path/boot/initramfs-linux.img" \
        "$deploy_path/boot/initramfs-$kver-fallback.img"; do
        [ -f "$candidate" ] && initramfs_src="$candidate" && break
    done
    if [ -f "$initramfs_src" ]; then
        cp -f "$initramfs_src" "$initramfs_dst"
    fi

    if [ ! -f "$initramfs_dst" ]; then
        if command -v dracut >/dev/null 2>&1; then
            dracut --force --no-hostonly --kver "$kver" --kernel-image "$vmlinuz_dst" "$initramfs_dst" 2>/dev/null || true
        elif command -v mkinitcpio >/dev/null 2>&1; then
            cp "$vmlinuz_src" "/boot/vmlinuz-$kver" 2>/dev/null || true
            mkinitcpio -k "$kver" -g "$initramfs_dst" 2>/dev/null || true
            rm -f "/boot/vmlinuz-$kver" 2>/dev/null || true
        fi
    fi
    [ ! -f "$initramfs_dst" ] && continue

    if [ "$count" -eq 1 ]; then
        title="Arch Linux - Omega"
    else
        title="Arch Linux - Alpha"
    fi
    bootcsum="${deploy_id%.*}"
    bootserial="${deploy_id##*.}"
    ostree_param="ostree=/ostree/boot.0/default/${bootcsum}/${bootserial}"
    bootlink_dir="$SYSROOT/ostree/boot.0/default/$bootcsum"
    mkdir -p "$bootlink_dir"
    ln -sfn "../../../deploy/default/deploy/$deploy_id" "$bootlink_dir/$bootserial"
    cmdline="root=UUID=$ROOT_UUID rw quiet splash loglevel=3 rd.udev.log_priority=3 $ostree_param"

    entry_file="$ESP/loader/entries/ostree-$deploy_id.conf"
    cat > "$entry_file" << BLSENTRY
title $title
version $kver
options $cmdline
linux /ostree/$deploy_id/vmlinuz-$kver
initrd /ostree/$deploy_id/initramfs-$kver.img
BLSENTRY
    echo "bls-sync: entry $deploy_id kernel $kver"
done

for entry in "$ESP/loader/entries/ostree-"*.conf; do
    [ ! -f "$entry" ] && continue
    id=$(basename "$entry" .conf | sed 's/^ostree-//')
    found=0
    for d in $deployments; do
        d=$(echo "$d" | tr -d '\n\r ')
        [ "$id" = "$d" ] && found=1 && break
    done
    [ "$found" = "0" ] && rm -f "$entry" && rm -rf "$ESP/ostree/$id" 2>/dev/null || true
done

[ ! -f "$ESP/loader/loader.conf" ] && printf "timeout 3\nconsole-mode max\ndefault @\n" > "$ESP/loader/loader.conf"
mount -o remount,ro "$SYSROOT" 2>/dev/null || true
"#;



fn build_network_page(sender: std::sync::mpsc::Sender<String>) -> (Box, Rc<dyn Fn()>) {
    let toast_overlay = ToastOverlay::new();
    let wrapper = Box::new(Orientation::Vertical, 0);

    // ── Content: icon + text ──
    let content = Box::new(Orientation::Vertical, 18);
    content.set_margin_top(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);
    content.set_margin_end(24);
    content.set_vexpand(true);
    content.set_valign(gtk::Align::Center);

    let offline_icon = Image::builder()
        .icon_name("network-wireless-offline-symbolic")
        .pixel_size(96)
        .halign(gtk::Align::Center)
        .margin_bottom(24)
        .build();
    offline_icon.set_opacity(0.5);

    let msg_label = Label::builder()
        .label("<b>Not connected to the internet</b>")
        .use_markup(true)
        .halign(gtk::Align::Center)
        .build();
    msg_label.add_css_class("title-2");

    let sub_label = Label::builder()
        .label("Open GNOME Settings to configure Wi-Fi")
        .halign(gtk::Align::Center)
        .wrap(true)
        .build();

    content.append(&offline_icon);
    content.append(&msg_label);
    content.append(&sub_label);

    // ── Footer ──
    let footer = Box::new(Orientation::Horizontal, 0);
    footer.set_margin_top(16);
    footer.set_margin_bottom(24);
    footer.set_margin_start(24);
    footer.set_margin_end(24);

    let settings_btn = Button::builder()
        .label("Open Network Settings")
        .hexpand(true)
        .css_classes(["suggested-action"])
        .build();

    footer.append(&settings_btn);

    // ── Assemble ──
    let page_box = Box::new(Orientation::Vertical, 0);
    page_box.append(&content);
    page_box.append(&footer);

    wrapper.append(&page_box);
    toast_overlay.set_child(Some(&wrapper));

    // Open Wi-Fi Settings in GNOME Control Center
    settings_btn.connect_clicked(|_| {
        let _ = std::process::Command::new("gnome-control-center")
            .arg("wifi")
            .spawn();
    });

    // Re-check connectivity every 3s to auto-advance when user connects
    let check_sender = sender.clone();
    glib::timeout_add_local(std::time::Duration::from_secs(3), move || {
        if nm::is_online() {
            let _ = check_sender.send("connected".to_string());
        }
        glib::ControlFlow::Continue
    });

    let trigger: Rc<dyn Fn()> = Rc::new(|| {});

    (wrapper, trigger)
}

fn main() {
    // Intercept CLI arguments
    let args: Vec<String> = env::args().collect();
    if args.len() > 1 && args[1] == "update" {
        println!("🚀 Initiating Arch Linux System Update...");
        println!("Please authenticate if prompted.");

        let status = std::process::Command::new("pkexec")
            .args(["bootc", "upgrade"])
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .status()
            .expect("Failed to launch bootc upgrade");
        
        if status.success() {
            println!("✅ System update completed successfully! Please reboot your system.");
        } else {
            println!("❌ System update failed with status: {}", status);
            println!("↩ Rolling back failed update...");
            let _ = std::process::Command::new("pkexec")
                .args(["bootc", "rollback"])
                .status();
            let _ = std::process::Command::new("pkexec")
                .args(["bash", "-c", BLS_SYNC_SCRIPT])
                .status();
            println!("↩ Rollback complete. Bootloader entries synchronized.");
        }
        
        return;
    }

    if args.len() > 1 && args[1] == "--check-update" {
        let _ = log_to_desktop("alga: checking for self-update...");
        match check_alga_update() {
            Ok(Some(version)) => {
                let msg = format!("alga {} is available", version);
                std::process::Command::new("notify-send")
                    .args(["Alga Update", &msg, "--icon=software-update-available"])
                    .status()
                    .ok();
            }
            Ok(None) => {}
            Err(e) => {
                let _ = log_to_desktop(&format!("alga check-update error: {}", e));
            }
        }
        return;
    }

    let is_installed_os = std::path::Path::new("/run/ostree-booted").exists();

    if is_installed_os {
        let app = Application::builder()
            .application_id("com.zamkara.alga.updater")
            .build();

        app.connect_startup(|_| {
            // Suppress Adwaita CSS deprecation warnings during init
            glib::log_set_handler(
                None,
                glib::LogLevels::LEVEL_WARNING,
                false, false,
                |_, _, _| {},
            );
            let _ = libadwaita::init();
        });

        app.connect_activate(build_updater_ui);
        app.run();
    } else {
        let app = Application::builder()
            .application_id("com.zamkara.alga")
            .build();

        app.connect_startup(|_| {
            glib::log_set_handler(
                None,
                glib::LogLevels::LEVEL_WARNING,
                false, false,
                |_, _, _| {},
            );
            let _ = libadwaita::init();
        });

        app.connect_activate(build_ui);
        app.run();
    }
}

fn build_updater_ui(app: &Application) {
    let provider = gtk::CssProvider::new();
    provider.load_from_data("
        .log-container, .log-container textview, .log-container text { 
            border-radius: 12px; 
        }
        .log-wrapper {
            border-radius: 12px;
            border: none;
        }
    ");
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().unwrap(),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );

    let window = ApplicationWindow::builder()
        .application(app)
        .title("Software Updater")
        .default_width(360)
        .default_height(420)
        .build();

    let main_box = Box::new(Orientation::Vertical, 0);
    let header_bar = HeaderBar::new();
    main_box.append(&header_bar);

    let stack = Stack::builder()
        .transition_type(StackTransitionType::SlideLeftRight)
        .build();

    let (net_sender, net_receiver) = std::sync::mpsc::channel::<String>();
    let (net_page, net_trigger) = build_network_page(net_sender.clone());
    stack.add_named(&net_page, Some("page0"));

    glib::idle_add_local(clone!(@weak stack => @default-return glib::ControlFlow::Continue, move || {
        while let Ok(msg) = net_receiver.try_recv() {
            if msg == "connected" {
                let current = stack.visible_child_name().unwrap_or_default().to_string();
                if current == "page0" {
                    stack.set_visible_child_name("page1");
                    return glib::ControlFlow::Break;
                }
            }
        }
        glib::ControlFlow::Continue
    }));

    stack.connect_visible_child_notify(clone!(@weak stack, @strong net_sender, @strong net_trigger => move |s| {
        let name = s.visible_child_name().unwrap_or_default().to_string();
        if name == "page0" && nm::is_online() {
            let _ = net_sender.send("connected".to_string());
        }
    }));

    let page1_box = Box::new(Orientation::Vertical, 0);
    let content_box = Box::new(Orientation::Vertical, 18);
    content_box.set_margin_top(32);
    content_box.set_margin_bottom(32);
    content_box.set_margin_start(32);
    content_box.set_margin_end(32);
    content_box.set_vexpand(true);

    let icon = Image::builder()
        .file("/usr/share/alga/check-for-update.svg")
        .pixel_size(128)
        .halign(gtk::Align::Center)
        .margin_bottom(12)
        .build();
    content_box.append(&icon);

    let title = Label::builder()
        .label("Software Updater")
        .css_classes(vec!["title-1".to_string()])
        .halign(gtk::Align::Center)
        .build();
    content_box.append(&title);

    let desc = Label::builder()
        .label("Check for available system updates.")
        .justify(gtk::Justification::Center)
        .wrap(true)
        .halign(gtk::Align::Center)
        .build();
    content_box.append(&desc);

    let progress_bar = ProgressBar::builder()
        .visible(false)
        .margin_top(12)
        .build();
    content_box.append(&progress_bar);

    let text_view = TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(12)
        .right_margin(12)
        .top_margin(12)
        .bottom_margin(12)
        .visible(false)
        .build();
    text_view.add_css_class("monospace");
    text_view.add_css_class("log-container");
    let scrolled = ScrolledWindow::builder()
        .child(&text_view)
        .vexpand(true)
        .min_content_height(120)
        .visible(false)
        .build();
    scrolled.add_css_class("log-wrapper");
    content_box.append(&scrolled);

    page1_box.append(&content_box);

    let footer = Box::new(Orientation::Horizontal, 0);
    footer.set_margin_top(16);
    footer.set_margin_bottom(24);
    footer.set_margin_start(24);
    footer.set_margin_end(24);

    let action_btn = Button::builder()
        .label("Check for Updates")
        .css_classes(vec!["suggested-action".to_string()])
        .hexpand(true)
        .build();
    footer.append(&action_btn);
    page1_box.append(&footer);

    let alga_sep = gtk::Separator::builder()
        .orientation(Orientation::Horizontal)
        .margin_start(24)
        .margin_end(24)
        .build();
    page1_box.append(&alga_sep);

    let alga_footer = Box::new(Orientation::Horizontal, 8);
    alga_footer.set_margin_top(8);
    alga_footer.set_margin_bottom(16);
    alga_footer.set_margin_start(24);
    alga_footer.set_margin_end(24);

    let alga_ver = Label::builder()
        .label(&format!("alga v{}", ALGA_VERSION))
        .css_classes(vec!["caption".to_string()])
        .halign(gtk::Align::Start)
        .hexpand(true)
        .build();
    alga_footer.append(&alga_ver);

    let alga_check_btn = Button::builder()
        .label("Check Self-Update")
        .css_classes(vec!["flat".to_string()])
        .build();
    alga_footer.append(&alga_check_btn);
    page1_box.append(&alga_footer);

    let alga_status = Label::builder()
        .css_classes(vec!["caption".to_string()])
        .halign(gtk::Align::Center)
        .margin_bottom(8)
        .build();
    page1_box.append(&alga_status);

    stack.add_named(&page1_box, Some("page1"));

    main_box.append(&stack);
    window.set_content(Some(&main_box));
    window.present();

    let state: Rc<RefCell<u8>> = Rc::new(RefCell::new(0));

    action_btn.connect_clicked(clone!(@weak action_btn, @weak progress_bar, @weak text_view, @weak scrolled, @weak desc, @weak icon, @strong state => move |_| {
        let s = *state.borrow();

        if s == 0 || s == 3 {
            *state.borrow_mut() = 99;
            action_btn.set_sensitive(false);
            action_btn.set_label("Checking...");
            desc.set_label("Checking for available system updates...");

            let (sender, receiver) = std::sync::mpsc::channel::<String>();

            std::thread::spawn(move || {
                let output = std::process::Command::new("pkexec")
                    .args(["bootc", "upgrade", "--check"])
                    .output();

                match output {
                    Ok(out) => {
                        let stdout = String::from_utf8_lossy(&out.stdout);
                        let stderr = String::from_utf8_lossy(&out.stderr);
                        let lower = format!("{}{}", stdout, stderr).to_lowercase();

                        if out.status.success() {
                            if lower.contains("no update available") || lower.contains("no changes") {
                                let _ = sender.send("UP_TO_DATE".to_string());
                            } else {
                                let _ = sender.send("UPDATE_AVAILABLE".to_string());
                            }
                        } else {
                            let _ = sender.send("CHECK_FAILED".to_string());
                        }
                    }
                    Err(_) => {
                        let _ = sender.send("CHECK_FAILED".to_string());
                    }
                }
            });

            glib::idle_add_local(clone!(@weak action_btn, @weak desc, @weak icon, @strong state => @default-return glib::ControlFlow::Continue, move || {
                while let Ok(msg) = receiver.try_recv() {
                    match msg.as_str() {
                        "UPDATE_AVAILABLE" => {
                            *state.borrow_mut() = 1;
                            action_btn.set_sensitive(true);
                            action_btn.set_label("Update Now");
                            desc.set_label("A new system update is available. Click Update Now to install.");
                            icon.set_file(Some("/usr/share/alga/update-available.svg"));
                        }
                        "UP_TO_DATE" => {
                            *state.borrow_mut() = 2;
                            action_btn.set_label("Up to Date");
                            action_btn.set_sensitive(false);
                            desc.set_label("Your system is currently up to date.");
                            icon.set_file(Some("/usr/share/alga/check-for-update.svg"));
                        }
                        "CHECK_FAILED" => {
                            *state.borrow_mut() = 3;
                            action_btn.set_label("Check Failed");
                            action_btn.set_sensitive(true);
                            desc.set_label("Unable to check for updates. Check your internet connection.");
                        }
                        _ => {}
                    }
                    return glib::ControlFlow::Break;
                }
                glib::ControlFlow::Continue
            }));
        } else if s == 1 {
            *state.borrow_mut() = 4;
            action_btn.set_sensitive(false);
            action_btn.set_label("Updating...");
            desc.set_label("Downloading and installing system update...");
            progress_bar.set_visible(true);
            scrolled.set_visible(true);
            text_view.set_visible(true);

            let (sender, receiver) = std::sync::mpsc::channel::<String>();

            let buffer = text_view.buffer();
            buffer.set_text("Starting update process...\n");

            std::thread::spawn(move || {
                let rt = tokio::runtime::Runtime::new().unwrap();
                rt.block_on(async {
                    log_to_desktop("[upgrade] Running: pkexec bootc upgrade");
                    let mut child = tokio::process::Command::new("pkexec")
                        .args(["bootc", "upgrade"])
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .spawn()
                        .expect("Failed to execute bootc upgrade");

                    let stdout = child.stdout.take().unwrap();
                    let stderr = child.stderr.take().unwrap();

                    let mut reader_out = BufReader::new(stdout).lines();
                    let mut reader_err = BufReader::new(stderr).lines();

                    let sender_clone1 = sender.clone();
                    let t1 = tokio::spawn(async move {
                        while let Ok(Some(line)) = reader_out.next_line().await {
                            let _ = sender_clone1.send(line);
                        }
                    });

                    let sender_clone2 = sender.clone();
                    let t2 = tokio::spawn(async move {
                        while let Ok(Some(line)) = reader_err.next_line().await {
                            let _ = sender_clone2.send(line);
                        }
                    });

                    let _ = tokio::join!(t1, t2);
                    let status = child.wait().await;
                    let ok = match status {
                        Ok(s) if s.success() => {
                            log_to_desktop("[upgrade] bootc upgrade succeeded");
                            true
                        },
                        _ => {
                            log_to_desktop("[upgrade] bootc upgrade failed");
                            false
                        }
                    };

                    if ok {
                        let _ = sender.send("Synchronizing bootloader entries...".to_string());
                        let _ = tokio::process::Command::new("pkexec")
                            .args(["bash", "-c", BLS_SYNC_SCRIPT])
                            .output()
                            .await;
                        let _ = sender.send("EOF_SUCCESS".to_string());
                    } else {
                        let _ = sender.send("Rolling back failed update...".to_string());
                        let _ = tokio::process::Command::new("pkexec")
                            .args(["bootc", "rollback"])
                            .output()
                            .await;
                        let _ = sender.send("Synchronizing bootloader entries...".to_string());
                        let _ = tokio::process::Command::new("pkexec")
                            .args(["bash", "-c", BLS_SYNC_SCRIPT])
                            .output()
                            .await;
                        let _ = sender.send("EOF_ERROR".to_string());
                    };
                });
            });

            glib::idle_add_local(clone!(@weak text_view, @weak progress_bar, @weak action_btn, @weak desc, @weak icon, @strong state => @default-return glib::ControlFlow::Continue, move || {
                while let Ok(text) = receiver.try_recv() {
                    if text == "EOF_SUCCESS" {
                        *state.borrow_mut() = 5;
                        progress_bar.set_fraction(1.0);
                        action_btn.set_label("Reboot Now");
                        action_btn.set_sensitive(true);
                        desc.set_label("System update installed. Reboot to apply changes.");
                        icon.set_file(Some("/usr/share/alga/ready-to-go.svg"));
                        log_to_desktop("[upgrade] EOF_SUCCESS: update completed.");
                        return glib::ControlFlow::Break;
                    } else if text == "EOF_ERROR" {
                        *state.borrow_mut() = 6;
                        progress_bar.set_fraction(1.0);
                        action_btn.set_label("Update Failed");
                        action_btn.set_sensitive(true);
                        desc.set_label("Update encountered an error. Check the log for details.");
                        log_to_desktop("[upgrade] EOF_ERROR: update failed.");
                        return glib::ControlFlow::Break;
                    }

                    if let Some(pct_pos) = text.rfind('%') {
                        let before = &text[..pct_pos];
                        if let Some(non_digit) = before.rfind(|c: char| !c.is_ascii_digit()) {
                            if let Ok(pct) = before[non_digit + 1..].parse::<f64>() {
                                progress_bar.set_fraction(pct / 100.0);
                            }
                        } else if let Ok(pct) = before.parse::<f64>() {
                            progress_bar.set_fraction(pct / 100.0);
                        }
                    }

                    log_to_desktop(&format!("[upgrade] {}", text));
                    let buffer = text_view.buffer();
                    let mut end_iter = buffer.end_iter();
                    buffer.insert(&mut end_iter, &format!("{}\n", text));

                    let mark = buffer.create_mark(None, &buffer.end_iter(), false);
                    text_view.scroll_to_mark(&mark, 0.0, false, 0.0, 1.0);
                }
                glib::ControlFlow::Continue
            }));
        }
    }));

    let alga_update_ver: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    alga_check_btn.connect_clicked(clone!(@weak alga_check_btn, @weak alga_status, @weak alga_ver, @strong alga_update_ver => move |_| {
        let pending = alga_update_ver.borrow().clone();
        if let Some(version) = pending {
            alga_check_btn.set_sensitive(false);
            alga_status.set_label(&format!("Downloading v{}...", version));
            let (sender, receiver) = std::sync::mpsc::channel::<String>();
            let ver = version.clone();
            std::thread::spawn(move || {
                match download_alga_update(&ver) {
                    Ok(_) => { let _ = sender.send("DONE".to_string()); }
                    Err(e) => { let _ = sender.send(format!("ERROR:{}", e)); }
                }
            });
            glib::idle_add_local(clone!(@weak alga_check_btn, @weak alga_status, @weak alga_ver, @strong alga_update_ver => @default-return glib::ControlFlow::Continue, move || {
                while let Ok(msg) = receiver.try_recv() {
                    if msg == "DONE" {
                        alga_status.set_markup("<b>Update downloaded. Restarting...</b>");
                        restart_alga();
                    } else if let Some(err) = msg.strip_prefix("ERROR:") {
                        alga_status.set_label(&format!("Download failed: {}", err));
                        alga_check_btn.set_label("Retry");
                        alga_check_btn.set_sensitive(true);
                    }
                    return glib::ControlFlow::Break;
                }
                glib::ControlFlow::Continue
            }));
        } else {
            alga_check_btn.set_sensitive(false);
            alga_status.set_label("Checking for alga updates...");
            let (sender, receiver) = std::sync::mpsc::channel::<String>();
            std::thread::spawn(move || {
                match check_alga_update() {
                    Ok(Some(version)) => { let _ = sender.send(format!("AVAILABLE:{}", version)); }
                    Ok(None) => { let _ = sender.send("UP_TO_DATE".to_string()); }
                    Err(e) => { let _ = sender.send(format!("ERROR:{}", e)); }
                }
            });
            glib::idle_add_local(clone!(@weak alga_check_btn, @weak alga_status, @strong alga_update_ver => @default-return glib::ControlFlow::Continue, move || {
                while let Ok(msg) = receiver.try_recv() {
                    if msg == "UP_TO_DATE" {
                        alga_status.set_label("Already up to date");
                        alga_check_btn.set_sensitive(true);
                    } else if let Some(ver) = msg.strip_prefix("AVAILABLE:") {
                        *alga_update_ver.borrow_mut() = Some(ver.to_string());
                        alga_status.set_markup(&format!("<b>Update available: v{}</b>", ver));
                        alga_check_btn.set_label("Update Alga");
                        alga_check_btn.set_sensitive(true);
                    } else if let Some(err) = msg.strip_prefix("ERROR:") {
                        alga_status.set_label(&format!("Check failed: {}", err));
                        alga_check_btn.set_sensitive(true);
                    }
                    return glib::ControlFlow::Break;
                }
                glib::ControlFlow::Continue
            }));
        }
    }));
}


fn build_ui(app: &Application) {
    let provider = gtk::CssProvider::new();
    provider.load_from_data("
        .log-container, .log-container textview, .log-container text { 
            border-radius: 12px; 
        }
        .log-wrapper {
            border-radius: 12px;
            border: none;
        }
    ");
    gtk::style_context_add_provider_for_display(
        &gtk::gdk::Display::default().unwrap(),
        &provider,
        gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
    );



    let window = ApplicationWindow::builder()
        .application(app)
        .title("Arch Linux Installer")
        .default_width(360)  // Very narrow wizard
        .default_height(660) // Taller to fit content
        .build();

    let main_box = Box::new(Orientation::Vertical, 0);
    let header_bar = HeaderBar::new();
    
    let back_btn = Button::builder()
        .icon_name("go-previous-symbolic")
        .visible(false)
        .build();
    back_btn.add_css_class("flat");
    header_bar.pack_start(&back_btn);
    
    main_box.append(&header_bar);

    let stack = Stack::builder()
        .transition_type(StackTransitionType::SlideLeftRight)
        .build();

    let target_disk = Rc::new(RefCell::new(String::new()));
    let target_variant = Rc::new(RefCell::new(String::new()));
    let target_zram = Rc::new(RefCell::new(String::from("auto")));
    let cancel_sender: Rc<RefCell<Option<oneshot::Sender<()>>>> = Rc::new(RefCell::new(None));
    let pulse_timeout: Rc<RefCell<Option<glib::SourceId>>> = Rc::new(RefCell::new(None));

    // --- Page 1: Disk Selection ---
    let page1_box = Box::new(Orientation::Vertical, 0);
    let content1 = Box::new(Orientation::Vertical, 18);
    content1.set_margin_top(16);
    content1.set_margin_bottom(24);
    content1.set_margin_start(24);
    content1.set_margin_end(24);
    content1.set_vexpand(true);
    
    let app_icon = Image::builder()
        .file("/usr/share/icons/MoreWaita/scalable/devices/drive-harddisk-solidstate.svg")
        .pixel_size(96)
        .halign(gtk::Align::Center)
        .margin_bottom(24)
        .build();
    
    let pref_group1 = PreferencesGroup::new();
    let host_drives = get_host_drives();
    let mut disk_radios: Vec<CheckButton> = Vec::new();
    let lsblk = std::process::Command::new("lsblk")
        .args(["-d", "-n", "-P", "-o", "NAME,SIZE,MODEL,RM,TRAN,TYPE"])
        .output();
        
    if let Ok(output) = lsblk {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if line.contains("TYPE=\"disk\"") && line.contains("RM=\"0\"") && !line.contains("TRAN=\"usb\"") 
                && !line.contains("NAME=\"loop") && !line.contains("NAME=\"zram") && !line.contains("NAME=\"ram") && !line.contains("NAME=\"sr") {
                let name = extract_val(line, "NAME");
                if host_drives.contains(&name) {
                    continue; // Skip the host's actively running drives
                }
                
                let size = extract_val(line, "SIZE");
                let model = extract_val(line, "MODEL");
                
                let display_title = if model.is_empty() { format!("Unknown Device (/dev/{})", name) } else { model };
                let display_subtitle = format!("/dev/{} - {}", name, size);
                let machine_name = format!("/dev/{}", name);
                
                let row = ActionRow::builder().title(&display_title).subtitle(&display_subtitle).build();
                let check = CheckButton::builder().build();
                check.set_widget_name(&machine_name);

                if let Some(first) = disk_radios.first() {
                    check.set_group(Some(first));
                }
                
                disk_radios.push(check.clone());
                row.add_prefix(&check);
                row.set_activatable_widget(Some(&check));
                pref_group1.add(&row);
            }
        }
    }
    if disk_radios.is_empty() {
        pref_group1.add(&ActionRow::builder().title("No physical drives found").build());
    }

    let title1 = Label::builder().label("<b>Welcome to ark OS</b>").use_markup(true).halign(gtk::Align::Center).build();
    title1.add_css_class("title-2");
    let subtitle1 = Label::builder().label("Please select the internal physical drive where you would like to install your new system. External drives are hidden for your safety.").wrap(true).justify(gtk::Justification::Fill).build();

    content1.append(&app_icon);
    content1.append(&title1);
    content1.append(&subtitle1);
    
    let spacer1 = Box::builder().vexpand(true).build();
    content1.append(&spacer1);
    content1.append(&pref_group1);
    
    let scroll1 = ScrolledWindow::builder().child(&content1).vexpand(true).build();
    page1_box.append(&scroll1);
    
    // Full width footer button
    let footer1 = Box::new(Orientation::Horizontal, 0);
    footer1.set_margin_top(16);
    footer1.set_margin_bottom(24);
    footer1.set_margin_start(24);
    footer1.set_margin_end(24);
    let next_btn1 = Button::builder().label("Next").css_classes(["suggested-action"]).hexpand(true).build();
    footer1.append(&next_btn1);
    page1_box.append(&footer1);
    stack.add_named(&page1_box, Some("page1"));

    let (net_sender, net_receiver) = std::sync::mpsc::channel::<String>();
    let (net_page, net_trigger) = build_network_page(net_sender.clone());
    stack.add_named(&net_page, Some("page2"));

    glib::idle_add_local(clone!(@weak stack => @default-return glib::ControlFlow::Continue, move || {
        while let Ok(msg) = net_receiver.try_recv() {
            if msg == "connected" {
                let current = stack.visible_child_name().unwrap_or_default().to_string();
                if current == "page2" {
                    stack.set_visible_child_name("page3");
                }
            }
        }
        glib::ControlFlow::Continue
    }));

    stack.connect_visible_child_notify(clone!(@weak stack, @strong net_sender, @strong net_trigger => move |s| {
        let name = s.visible_child_name().unwrap_or_default().to_string();
        if name == "page2" && nm::is_online() {
            let _ = net_sender.send("connected".to_string());
        }
    }));

    // --- Page 3: System Configuration ---
    let page2_box = Box::new(Orientation::Vertical, 0);
    let content2 = Box::new(Orientation::Vertical, 12);
    content2.set_margin_top(24);
    content2.set_margin_bottom(24);
    content2.set_margin_start(24);
    content2.set_margin_end(24);
    content2.set_vexpand(true);
    
    let title2 = Label::builder().label("<b>System Configuration</b>").use_markup(true).halign(gtk::Align::Start).build();
    title2.add_css_class("title-2");
    
    // --- Kernel Selection ---
    let grp_kernel = PreferencesGroup::builder().description("Choose the core kernel that best suits your workflow.").build();
    let k_linux = CheckButton::builder().build();
    let k_zen = CheckButton::builder().group(&k_linux).build();
    let k_lts = CheckButton::builder().group(&k_linux).build();
    let k_hardened = CheckButton::builder().group(&k_linux).build();
    
    let row_kl = ActionRow::builder().title("Ark Standard").subtitle("Default kernel").build();
    row_kl.add_prefix(&k_linux);
    row_kl.set_activatable_widget(Some(&k_linux));
    k_linux.set_active(true);
    
    let row_kz = ActionRow::builder().title("Ark Zen").subtitle("Desktop and gaming optimized").build();
    row_kz.add_prefix(&k_zen);
    row_kz.set_activatable_widget(Some(&k_zen));
    
    let row_klts = ActionRow::builder().title("Ark LTS").subtitle("Maximum stability").build();
    row_klts.add_prefix(&k_lts);
    row_klts.set_activatable_widget(Some(&k_lts));
    
    let row_kh = ActionRow::builder().title("Ark Hardened").subtitle("Security focused").build();
    row_kh.add_prefix(&k_hardened);
    row_kh.set_activatable_widget(Some(&k_hardened));
    
    grp_kernel.add(&row_kl);
    grp_kernel.add(&row_kz);
    grp_kernel.add(&row_klts);
    grp_kernel.add(&row_kh);
    
    // --- Graphics Switch ---
    let row_gnv = ActionRow::builder()
        .title("NVIDIA Drivers")
        .subtitle("Proprietary high performance driver")
        .build();
    let nv_switch = Switch::builder().active(false).valign(gtk::Align::Center).build();
    row_gnv.add_suffix(&nv_switch);
    grp_kernel.add(&row_gnv);
    
    // --- zRAM Swap Size ---
    let grp_zram = PreferencesGroup::builder().description("Select the amount of compressed RAM to use as swap space.").build();
    let model_zram = gtk::StringList::new(&["Disabled", "2 GB", "4 GB", "8 GB", "16 GB", "Auto"]);
    let combo_zram = libadwaita::ComboRow::builder().title("Swap Size").model(&model_zram).selected(5).build();
    grp_zram.add(&combo_zram);

    content2.append(&title2);
    
    let scroll_box = Box::new(Orientation::Vertical, 12);
    let scroll_conf = ScrolledWindow::builder().child(&scroll_box).vexpand(true).build();
    scroll_box.append(&grp_kernel);
    scroll_box.append(&grp_zram);
    
    content2.append(&scroll_conf);
    page2_box.append(&content2);

    let footer2 = Box::new(Orientation::Horizontal, 0);
    footer2.set_margin_top(16);
    footer2.set_margin_bottom(24);
    footer2.set_margin_start(24);
    footer2.set_margin_end(24);
    let next_btn2 = Button::builder().label("Next").css_classes(["suggested-action"]).hexpand(true).build();
    footer2.append(&next_btn2);
    page2_box.append(&footer2);
    stack.add_named(&page2_box, Some("page3"));

    // --- Page 3: Detailed Confirmation ---
    let page3_box = Box::new(Orientation::Vertical, 0);
    let content3 = Box::new(Orientation::Vertical, 18);
    content3.set_margin_top(24);
    content3.set_margin_bottom(24);
    content3.set_margin_start(24);
    content3.set_margin_end(24);
    content3.set_vexpand(true);
    
    let title3 = Label::builder().label("<b>Terms of Installation</b>").use_markup(true).halign(gtk::Align::Start).build();
    title3.add_css_class("title-2");
    
    let info_text = "<b>Action Cannot Be Undone</b>\n\n\
                     You are about to install ark OS onto your physical drive. \
                     By proceeding, you authorize the installer to reformat the entire device.\n\n\
                     All partitions will be destroyed and all existing operating systems will be erased. \
                     Furthermore, all personal files, documents, and data on this drive will be permanently lost.\n\n\
                     Please ensure you have backed up any important data to an external drive or cloud storage before continuing.";
                     
    let info_label = Label::builder()
        .label(info_text)
        .use_markup(true)
        .wrap(true)
        .justify(gtk::Justification::Fill)
        .build();
        
    let pref_group3 = PreferencesGroup::new();
    let ack_row = ActionRow::builder().title("I understand that all data on my drive will be completely erased").build();
    ack_row.set_title_lines(0);
    let ack_check = CheckButton::new();
    ack_row.add_prefix(&ack_check);
    ack_row.set_activatable_widget(Some(&ack_check));
    pref_group3.add(&ack_row);

    let grub_row = ActionRow::builder()
        .title("Install GRUB Bootloader")
        .subtitle("Optional fallback bootloader. If disabled, only systemd-boot will be installed.")
        .build();
    let grub_switch = Switch::builder().active(false).valign(gtk::Align::Center).build();
    grub_row.add_suffix(&grub_switch);
    pref_group3.add(&grub_row);
    
    content3.append(&title3);
    content3.append(&info_label);
    
    // Add spacer so checkbox is at the bottom of the scrollable area
    let spacer = Box::builder().vexpand(true).build();
    content3.append(&spacer);
    content3.append(&pref_group3);
    
    let scroll3 = ScrolledWindow::builder().child(&content3).vexpand(true).build();
    page3_box.append(&scroll3);
    
    let footer3 = Box::new(Orientation::Horizontal, 0);
    footer3.set_margin_top(16);
    footer3.set_margin_bottom(24);
    footer3.set_margin_start(24);
    footer3.set_margin_end(24);
    let erase_btn3 = Button::builder().label("Erase & Install").css_classes(["destructive-action"]).hexpand(true).sensitive(false).build();
    footer3.append(&erase_btn3);
    page3_box.append(&footer3);
    
    ack_check.connect_toggled(clone!(@weak erase_btn3 => move |cb| {
        erase_btn3.set_sensitive(cb.is_active());
    }));
    
    stack.add_named(&page3_box, Some("page4"));

    // --- Page 5: Progress (Rounded Log Window) ---
    let page4_box = Box::new(Orientation::Vertical, 0);
    let content4 = Box::new(Orientation::Vertical, 18);
    content4.set_margin_top(24);
    content4.set_margin_bottom(24);
    content4.set_margin_start(24);
    content4.set_margin_end(24);
    content4.set_vexpand(true);
    
    let title4 = Label::builder().label("<b>Installing ark OS...</b>").use_markup(true).halign(gtk::Align::Start).build();
    title4.add_css_class("title-2");
    
    let progress_bar = ProgressBar::builder().show_text(false).build();
    
    let text_view = TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(12)
        .right_margin(12)
        .top_margin(12)
        .bottom_margin(12)
        .build();
    text_view.add_css_class("monospace");
    text_view.add_css_class("log-container");
    
    // Make the scrolled window look like a card with rounded corners
    let scroll4 = ScrolledWindow::builder()
        .child(&text_view)
        .vexpand(true)
        .build();
    scroll4.add_css_class("log-wrapper");
    
    content4.append(&title4);
    content4.append(&progress_bar);
    content4.append(&scroll4);
    page4_box.append(&content4);
    
    let footer4 = Box::new(Orientation::Horizontal, 0);
    footer4.set_margin_top(16);
    footer4.set_margin_bottom(24);
    footer4.set_margin_start(24);
    footer4.set_margin_end(24);
    let cancel_btn = Button::builder().label("Cancel Install").css_classes(["destructive-action"]).hexpand(true).build();
    footer4.append(&cancel_btn);
    page4_box.append(&footer4);
    
    stack.add_named(&page4_box, Some("page5"));

    // --- Page 6: Success ---
    let page5_box = Box::new(Orientation::Vertical, 0);
    let content5 = Box::new(Orientation::Vertical, 18);
    content5.set_margin_top(24);
    content5.set_margin_bottom(24);
    content5.set_margin_start(24);
    content5.set_margin_end(24);
    content5.set_vexpand(true);
    content5.set_halign(gtk::Align::Center);
    content5.set_valign(gtk::Align::Center);

    // L3: ready-to-go icon — bundled at /usr/share/alga/ready-to-go.svg
    let success_icon = Image::builder()
        .file("/usr/share/alga/ready-to-go.svg")
        .pixel_size(128)
        .halign(gtk::Align::Center)
        .margin_bottom(12)
        .build();
    
    let title5 = Label::builder().label("<b>Installation Complete!</b>").use_markup(true).build();
    title5.add_css_class("title-1");
    let success_lbl = Label::new(Some("Ark Project is successfully installed. Reboot to start using your new system."));
    success_lbl.set_wrap(true);
    success_lbl.set_justify(gtk::Justification::Center);
    content5.append(&success_icon);
    content5.append(&title5);
    content5.append(&success_lbl);
    page5_box.append(&content5);
    
    let footer5 = Box::new(Orientation::Horizontal, 12);
    footer5.set_homogeneous(true); // Make both buttons equal width
    footer5.set_margin_top(16);
    footer5.set_margin_bottom(24);
    footer5.set_margin_start(24);
    footer5.set_margin_end(24);
    let stay_btn = Button::builder().label("Stay Live").hexpand(true).build();
    let reboot_btn = Button::builder().label("Reboot").css_classes(["suggested-action"]).hexpand(true).build();
    footer5.append(&stay_btn);
    footer5.append(&reboot_btn);
    page5_box.append(&footer5);
    stack.add_named(&page5_box, Some("page6"));

    // --- Navigation Logic ---
    
    stack.connect_visible_child_notify(clone!(@weak back_btn => move |s| {
        let current = s.visible_child_name().unwrap_or_default();
        back_btn.set_visible(current == "page2" || current == "page3" || current == "page4");
    }));

    back_btn.connect_clicked(clone!(@weak stack => move |_| {
        let current = stack.visible_child_name().unwrap_or_default();
        if current == "page2" {
            stack.set_visible_child_name("page1");
        } else if current == "page3" {
            stack.set_visible_child_name("page1");
        } else if current == "page4" {
            stack.set_visible_child_name("page3");
        }
    }));
    
    next_btn1.connect_clicked(clone!(@weak stack, @strong disk_radios, @strong target_disk => move |_| {
        let mut selected = String::new();
        for cb in &disk_radios {
            if cb.is_active() {
                selected = cb.widget_name().to_string();
            }
        }
        if !selected.is_empty() {
            *target_disk.borrow_mut() = selected;
            stack.set_visible_child_name("page2");
        }
    }));
    
    next_btn2.connect_clicked(clone!(@weak stack, @strong target_variant, @strong target_zram, @weak k_zen, @weak k_lts, @weak k_hardened, @weak nv_switch, @weak combo_zram => move |_| {
        let kernel = if k_zen.is_active() { "zen" }
                     else if k_lts.is_active() { "lts" }
                     else if k_hardened.is_active() { "hardened" }
                     else { "linux" };
                     
        let is_nvidia = nv_switch.is_active();
        
        let var_name = match (kernel, is_nvidia) {
            ("linux", false) => "ark".to_string(),
            ("linux", true) => "ark-nvidia".to_string(),
            (k, false) => format!("ark-{}", k),
            (k, true) => format!("ark-{}-nvidia", k),
        };
        
        *target_variant.borrow_mut() = format!("ghcr.io/zamkara/ark-image:{}", var_name);
        
        let zram_idx = combo_zram.selected();
        *target_zram.borrow_mut() = match zram_idx {
            0 => "disabled".to_string(),
            1 => "2048".to_string(),
            2 => "4096".to_string(),
            3 => "8192".to_string(),
            4 => "16384".to_string(),
            _ => "auto".to_string(),
        };
        
        stack.set_visible_child_name("page4");
    }));
    
    cancel_btn.connect_clicked(clone!(@strong cancel_sender, @weak stack, @weak cancel_btn => move |_| {
        if let Some(sender) = cancel_sender.borrow_mut().take() {
            let _ = sender.send(()); // Send kill signal
        } else {
            // Act as Back button if installation is already finished/failed
            stack.set_visible_child_name("page1");
            cancel_btn.set_label("Cancel Install");
            cancel_btn.add_css_class("destructive-action");
            cancel_btn.remove_css_class("suggested-action");
        }
    }));
    
    erase_btn3.connect_clicked(clone!(@weak stack, @weak text_view, @weak progress_bar, @weak cancel_btn, @weak title4, @strong target_disk, @strong target_variant, @strong target_zram, @strong cancel_sender, @strong pulse_timeout, @weak grub_switch => move |_| {
        // Reset UI state for installation/retry
        text_view.buffer().set_text("");
        progress_bar.remove_css_class("error");
        progress_bar.remove_css_class("success");
        progress_bar.set_fraction(0.0);
        title4.set_label("<b>0% Starting installation...</b>");

        // Clear and initialize verbose log file on Desktop
        if let Ok(home) = std::env::var("HOME") {
            let desktop_log = std::path::PathBuf::from(home).join("Desktop").join("log.txt");
            if let Ok(mut file) = std::fs::File::create(&desktop_log) {
                use std::io::Write;
                let _ = writeln!(file, "=== Ark Project Installation Verbose Log ===");
            }
        }

        stack.set_visible_child_name("page5");
        cancel_btn.set_visible(true);
        cancel_btn.set_label("Cancel Install");
        cancel_btn.add_css_class("destructive-action");
        cancel_btn.remove_css_class("suggested-action");
        
        let source_id = glib::timeout_add_local(std::time::Duration::from_millis(100), clone!(@weak progress_bar => @default-return glib::ControlFlow::Break, move || {
            progress_bar.pulse();
            glib::ControlFlow::Continue
        }));
        *pulse_timeout.borrow_mut() = Some(source_id);
        
        let disk = target_disk.borrow().clone();
        let variant = target_variant.borrow().clone();
        let zram_val = target_zram.borrow().clone();
        
        let (sender, receiver) = std::sync::mpsc::channel::<String>();
        let (kill_tx, mut kill_rx) = oneshot::channel::<()>();
        *cancel_sender.borrow_mut() = Some(kill_tx);
        
        glib::idle_add_local(clone!(@weak text_view, @weak progress_bar, @weak stack, @weak cancel_btn, @weak title4, @strong cancel_sender, @strong pulse_timeout => @default-return glib::ControlFlow::Continue, move || {
            while let Ok(msg) = receiver.try_recv() {
                // Append raw log line to ~/Desktop/log.txt
                if let Ok(home) = std::env::var("HOME") {
                    let desktop_log = std::path::PathBuf::from(home).join("Desktop").join("log.txt");
                    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(&desktop_log) {
                        use std::io::Write;
                        let _ = writeln!(file, "{}", msg);
                    }
                }

                if msg.starts_with("EOF_") {
                    if let Some(id) = pulse_timeout.borrow_mut().take() {
                        id.remove();
                    }
                }
                
                if msg == "EOF_SUCCESS" {
                    stack.set_visible_child_name("page6");
                    return glib::ControlFlow::Break;
                } else if msg == "EOF_CANCEL" {
                    text_view.buffer().insert(&mut text_view.buffer().end_iter(), "\n[Installation Cancelled]\n");
                    stack.set_visible_child_name("page1"); 
                    return glib::ControlFlow::Break;
                } else if msg == "EOF_ERROR" {
                    progress_bar.add_css_class("error");
                    
                    let _ = cancel_sender.borrow_mut().take();
                    cancel_btn.set_label("Back to Menu");
                    cancel_btn.remove_css_class("destructive-action");
                    cancel_btn.add_css_class("suggested-action");
                    
                    text_view.buffer().insert(&mut text_view.buffer().end_iter(), "\n[Installation Failed]\n");
                    return glib::ControlFlow::Break;
                }
                
                let (pct, clean_msg) = match sanitize_log(&msg) {
                    Some((p, m)) => (p, m),
                    None => continue,
                };
                
                if let Some(p) = pct {
                    title4.set_label(&format!("<b>{}% Installing...</b>", p));
                }
                
                let buffer = text_view.buffer();
                let mut iter = buffer.end_iter();
                buffer.insert(&mut iter, &format!("{}\n", clean_msg));
                
                let mark = buffer.create_mark(None, &buffer.end_iter(), false);
                text_view.scroll_to_mark(&mark, 0.0, false, 0.0, 0.0);
            }
            glib::ControlFlow::Continue
        }));
        
        let install_grub = grub_switch.is_active();

        std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async {
                let bootc_cmd = format!(
                    "killall -9 bootc skopeo 2>/dev/null || true; \
                     for p in {disk}*; do umount -l $p 2>/dev/null || true; done; \
                     umount -l /run/bootc/mounts/rootfs 2>/dev/null || true; \
                     btrfs device scan --forget 2>/dev/null || true; \
                     wipefs -af {disk}* || true; \
                     dd if=/dev/zero of={disk} bs=1M count=1 status=none || true; \
                     udevadm settle 2>/dev/null || true; \
                     sleep 1 || true; \
                     bootc install to-disk --wipe --filesystem btrfs --bootloader none --source-imgref docker://{variant} {disk} && \
                     ROOT_PART=$(lsblk -rno PATH,FSTYPE {disk} | grep -i 'btrfs' | head -n1 | awk '{{print $1}}'); \
                     mount $ROOT_PART /mnt && \
                     DEPLOY_ETC=$(ls -d /mnt/ostree/deploy/default/deploy/*/etc | head -n 1) && \
                     mkdir -p $DEPLOY_ETC/systemd && \
                     if [ \"{zram}\" != \"disabled\" ]; then \
                       echo \"[zram0]\" > $DEPLOY_ETC/systemd/zram-generator.conf; \
                       echo \"compression-algorithm = zstd\" >> $DEPLOY_ETC/systemd/zram-generator.conf; \
                       if [ \"{zram}\" = \"auto\" ]; then \
                         echo \"zram-size = ram / 2\" >> $DEPLOY_ETC/systemd/zram-generator.conf; \
                       else \
                         echo \"zram-size = {zram}\" >> $DEPLOY_ETC/systemd/zram-generator.conf; \
                       fi; \
                     else \
                       rm -f $DEPLOY_ETC/systemd/zram-generator.conf; \
                     fi && \
                     umount -l /mnt",
                    disk = disk,
                    variant = variant,
                    zram = zram_val
                );
                
                let mut child_install = tokio::process::Command::new("pkexec")
                    .args(["bash", "-c", &bootc_cmd])
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("Failed to spawn pkexec bootc install");
                    
                let mut stdout_inst = BufReader::new(child_install.stdout.take().unwrap()).lines();
                let mut stderr_inst = BufReader::new(child_install.stderr.take().unwrap()).lines();

                loop {
                    tokio::select! {
                        _ = &mut kill_rx => {
                            let _ = child_install.kill().await;
                            
                            let _ = sender.send("Cleaning up and formatting drive to unallocated state...".to_string());
                            let cleanup_cmd = format!(
                                "killall -9 bootc skopeo 2>/dev/null || true; for p in {}*; do umount -l $p 2>/dev/null || true; done; btrfs device scan --forget 2>/dev/null || true; wipefs -af {}* 2>/dev/null || true; dd if=/dev/zero of={} bs=1M count=10 2>/dev/null || true; partprobe {} 2>/dev/null || true", 
                                disk, disk, disk, disk
                            );
                            let _ = tokio::process::Command::new("pkexec")
                                .args(["bash", "-c", &cleanup_cmd])
                                .output()
                                .await;
                                
                            let _ = sender.send("EOF_CANCEL".to_string());
                            return;
                        }
                        line = stdout_inst.next_line() => {
                            match line {
                                Ok(Some(l)) => { let _ = sender.send(l); }
                                Ok(None) => break,
                                Err(_) => break,
                            }
                        }
                        line = stderr_inst.next_line() => {
                            if let Ok(Some(l)) = line { let _ = sender.send(l); }
                        }
                    }
                }
                
                let status = child_install.wait().await;
                match status {
                    Ok(s) if s.success() => {
                        let _ = sender.send("95% Installing bootloader...".to_string());
                        let bootloader_cmd = format!(
                            "set -e; \
                             EFI_PART=$(lsblk -rno PATH,FSTYPE {disk} | grep -i 'vfat' | head -n1 | awk '{{print $1}}'); \
                             BOOT_PART=$(lsblk -rno PATH,PARTTYPE {disk} | grep -i 'bc13c2ff-59e6-4262-a352-b275fd6f7172' | head -n1 | awk '{{print $1}}'); \
                             ROOT_PART=$(lsblk -rno PATH,FSTYPE {disk} | grep -i 'btrfs' | head -n1 | awk '{{print $1}}'); \
                             [ -z \"$EFI_PART\" ] && echo 'Error: EFI partition not found' && exit 1; \
                             [ -z \"$ROOT_PART\" ] && echo 'Error: Root partition not found' && exit 1; \
                             mkdir -p /tmp/root_mnt /tmp/efi_mnt; \
                             umount -l /tmp/root_mnt/boot/efi 2>/dev/null || true; \
                             umount -l /tmp/root_mnt/boot 2>/dev/null || true; \
                             umount -l /tmp/root_mnt 2>/dev/null || true; \
                             umount -l /tmp/efi_mnt 2>/dev/null || true; \
                             mount $ROOT_PART /tmp/root_mnt; \
                             if [ -n \"$BOOT_PART\" ]; then \
                               mkdir -p /tmp/root_mnt/boot; \
                               mount $BOOT_PART /tmp/root_mnt/boot; \
                             fi; \
                             mount $EFI_PART /tmp/efi_mnt; \
                             DEPLOY_PATH=$(find /tmp/root_mnt/ostree/deploy/default/deploy -maxdepth 1 -name '*.0' -type d | head -n1); \
                             [ -z \"$DEPLOY_PATH\" ] && echo 'Error: Deploy path not found' && exit 1; \
                             mkdir -p \"$DEPLOY_PATH/sysroot\" \"$DEPLOY_PATH/ostree\"; \
                             sed -i 's/transient=true/transient=false/g' /tmp/root_mnt/ostree/repo/config 2>/dev/null || true; \
                                 if [ \"{grub}\" = \"true\" ]; then \
                                   sed -i 's/bootloader=none/bootloader=grub2/' /tmp/root_mnt/ostree/repo/config; \
                                   grub-install --target=x86_64-efi --efi-directory=/tmp/efi_mnt --bootloader-id=ARCHLINUX --boot-directory=/tmp/root_mnt/boot --recheck; \
                                   ROOT_UUID=$(blkid -s UUID -o value \"$ROOT_PART\"); \
                                   VMLINUZ=$(find /tmp/root_mnt/boot/ostree -maxdepth 2 -name 'vmlinuz-*' -type f 2>/dev/null | head -n1); \
                                   INITRAMFS=$(find /tmp/root_mnt/boot/ostree -maxdepth 2 -name 'initramfs-*' -type f 2>/dev/null | head -n1); \
                                   if [ -z \"$VMLINUZ\" ]; then \
                                     echo 'Error: Kernel not found in /boot/ostree' && exit 1; \
                                   fi; \
                                   OSTREE_PARAM=$(grep -o 'ostree=[^ ]*' /tmp/root_mnt/boot/loader/entries/ostree-*.conf 2>/dev/null | head -n1) || true; \
                                   [ -z \"$OSTREE_PARAM\" ] && OSTREE_PARAM=\"ostree=0\"; \
                                   BOOT_PART_UUID=$(blkid -s UUID -o value \"$BOOT_PART\" 2>/dev/null) || true; \
                                   if [ -n \"$BOOT_PART_UUID\" ]; then \
                                     KERNEL_REL=$(echo \"$VMLINUZ\" | sed 's|/tmp/root_mnt/boot||'); \
                                     INIT_REL=$(echo \"$INITRAMFS\" | sed 's|/tmp/root_mnt/boot||'); \
                                     {{ \
                                       echo 'set default=0'; \
                                       echo 'set timeout=5'; \
                                       echo 'menuentry \"Arch Linux - Alpha\" {{'; \
                                       echo '    search --no-floppy --fs-uuid '\"$ROOT_UUID\"' --set=root'; \
                                       echo '    search --no-floppy --fs-uuid '\"$BOOT_PART_UUID\"' --set=boot_root'; \
                                       echo '    linux ($boot_root)'\"$KERNEL_REL\"' root=UUID='\"$ROOT_UUID\"' rw quiet splash loglevel=3 rd.udev.log_priority=3 vt.global_cursor_default=0 '\"$OSTREE_PARAM\"''; \
                                       echo '    initrd ($boot_root)'\"$INIT_REL\"''; \
                                       echo '}}'; \
                                     }} > /tmp/root_mnt/boot/grub/grub.cfg; \
                                   else \
                                     KERNEL_REL=$(echo \"$VMLINUZ\" | sed 's|/tmp/root_mnt||'); \
                                     INIT_REL=$(echo \"$INITRAMFS\" | sed 's|/tmp/root_mnt||'); \
                                     {{ \
                                       echo 'set default=0'; \
                                       echo 'set timeout=5'; \
                                       echo 'menuentry \"Arch Linux - Alpha\" {{'; \
                                       echo '    search --no-floppy --fs-uuid '\"$ROOT_UUID\"' --set=root'; \
                                       echo '    linux '\"$KERNEL_REL\"' root=UUID='\"$ROOT_UUID\"' rw quiet splash loglevel=3 rd.udev.log_priority=3 vt.global_cursor_default=0 '\"$OSTREE_PARAM\"''; \
                                       echo '    initrd '\"$INIT_REL\"''; \
                                       echo '}}'; \
                                     }} > /tmp/root_mnt/boot/grub/grub.cfg; \
                                   fi; \
                             else \
                               bootctl install --esp-path=/tmp/efi_mnt --boot-path=/tmp/root_mnt/boot 2>/dev/null || bootctl install --esp-path=/tmp/efi_mnt --boot-path=/tmp/root_mnt/boot --no-variables 2>/dev/null || true; \
                               mkdir -p /tmp/efi_mnt/EFI/BOOT /tmp/efi_mnt/EFI/systemd /tmp/efi_mnt/loader; \
                               cp /usr/lib/systemd/boot/efi/systemd-bootx64.efi /tmp/efi_mnt/EFI/BOOT/BOOTX64.EFI 2>/dev/null || true; \
                               cp /usr/lib/systemd/boot/efi/systemd-bootx64.efi /tmp/efi_mnt/EFI/systemd/systemd-bootx64.efi 2>/dev/null || true; \
                               if [ ! -f /tmp/efi_mnt/loader/loader.conf ]; then \
                                 echo \"timeout 3\" > /tmp/efi_mnt/loader/loader.conf; \
                                 echo \"console-mode max\" >> /tmp/efi_mnt/loader/loader.conf; \
                               fi; \
                               if [ -d \"/tmp/root_mnt/boot/ostree\" ]; then \
                                 mkdir -p /tmp/efi_mnt/ostree; \
                                 cp -r /tmp/root_mnt/boot/ostree/* /tmp/efi_mnt/ostree/ 2>/dev/null || true; \
                               fi; \
                               if [ -d \"/tmp/root_mnt/boot/loader/entries\" ]; then \
                                 mkdir -p /tmp/efi_mnt/loader/entries; \
                                 cp /tmp/root_mnt/boot/loader/entries/*.conf /tmp/efi_mnt/loader/entries/ 2>/dev/null || true; \
                                 sed -i 's|/boot/ostree|/ostree|g' /tmp/efi_mnt/loader/entries/*.conf 2>/dev/null || true; \
                               fi; \
                             fi; \
                             umount -l /tmp/efi_mnt 2>/dev/null || true; \
                             umount -l /tmp/root_mnt/boot 2>/dev/null || true; \
                             umount -l /tmp/root_mnt 2>/dev/null || true",
                            disk = disk,
                            grub = if install_grub { "true" } else { "false" }
                        );
                        let _ = tokio::process::Command::new("pkexec")
                            .args(["bash", "-c", &bootloader_cmd])
                            .output()
                            .await;
                        let _ = sender.send("EOF_SUCCESS".to_string());
                    },
                    _ => {
                        let _ = sender.send("EOF_ERROR".to_string());
                    }
                }
            });
        });
    }));
    
    stay_btn.connect_clicked(|_| {
        std::process::exit(0);
    });
    
    reboot_btn.connect_clicked(|_| {
        let _ = std::process::Command::new("sudo").arg("reboot").status();
    });

    main_box.append(&stack);
    window.set_content(Some(&main_box));
    window.present();
}

fn extract_val(line: &str, key: &str) -> String {
    let k = format!("{}=\"", key);
    if let Some(start) = line.find(&k) {
        let sub = &line[start + k.len()..];
        if let Some(end) = sub.find('"') {
            return sub[..end].to_string();
        }
    }
    String::new()
}

fn get_host_drives() -> Vec<String> {
    let mut drives = Vec::new();
    if let Ok(findmnt) = std::process::Command::new("findmnt").args(["-n", "-v", "-o", "SOURCE", "/"]).output() {
        let source = String::from_utf8_lossy(&findmnt.stdout).trim().to_string();
        if !source.is_empty() {
            if let Ok(lsblk) = std::process::Command::new("lsblk").args(["-s", "-n", "-P", "-o", "NAME,TYPE", &source]).output() {
                let stdout = String::from_utf8_lossy(&lsblk.stdout);
                for line in stdout.lines() {
                    if line.contains("TYPE=\"disk\"") {
                        let name = extract_val(line, "NAME");
                        if !name.is_empty() {
                            drives.push(name);
                        }
                    }
                }
            }
        }
    }
    drives
}

fn sanitize_log(raw: &str) -> Option<(Option<u32>, String)> {
    if raw.trim().is_empty() {
        return None;
    }
    
    let trimmed = raw.trim();
    let lower = trimmed.to_lowercase();
    
    let mut extracted_pct = None;
    if let Some(idx) = lower.find('%') {
        let mut start = idx;
        let bytes = lower.as_bytes();
        while start > 0 && bytes[start - 1].is_ascii_digit() {
            start -= 1;
        }
        if start < idx {
            if let Ok(val) = lower[start..idx].parse::<u32>() {
                extracted_pct = Some(val);
            }
        }
    }
    
    let hide_prefixes = [
        "Wiping",
        "Block setup:",
        "Size:",
        "Serial:",
        "Model:",
        "Partitions:",
        "Disk /dev",
        "Disk model:",
        "Units:",
        "Sector size",
        "I/O size",
        ">>> Script header",
        "New situation:",
        "Disklabel type:",
        "Disk identifier:",
        "Device", // matches "Device       Start"
        "The partition table has been altered",
        "Calling ioctl()",
        "Syncing disks",
        "> mkfs",
        "layers already present",
        "Bootloader:",
        "Checking that no-one is using this disk",
        "/dev/",
        "program: \"",
        "args: [",
        "create_pidfd:",
        "\"/dev/"
    ];
    
    for prefix in hide_prefixes.iter() {
        if trimmed.starts_with(prefix) {
            return None;
        }
    }
    
    let hide_exact = [
        "}",
        "],",
        "\"wipefs\",",
        "\"-a\","
    ];
    for ext in hide_exact.iter() {
        if trimmed == *ext {
            return None;
        }
    }
    
    if lower.contains("bytes were erased") || lower.contains("calling ioctl") || lower.contains("failed to run command") || lower.contains("command {") {
        return None;
    }

    // 2. Whitelist and translate friendly messages
    if lower.contains("installing image:") {
        return Some((Some(5), "Starting installation process...".to_string()));
    }
    if lower.contains("created a new gpt disklabel") {
        return Some((Some(15), "Configuring partition tables...".to_string()));
    }
    if lower.contains("creating root filesystem") {
        return Some((Some(30), "Formatting system partitions...".to_string()));
    }
    if lower.contains("creating esp filesystem") {
        return Some((Some(40), "Formatting boot partition...".to_string()));
    }
    if lower.contains("initializing ostree layout") {
        return Some((Some(50), "Initializing immutable system layout...".to_string()));
    }
    if lower.contains("deploying container image") {
        return Some((Some(70), "Deploying operating system image (this may take a while)...".to_string()));
    }
    
    // 3. Error handling
    if lower.contains("error:") || lower.contains("failed") {
        if lower.contains("network is unreachable") || lower.contains("unexpected end of file") {
            return Some((None, "Installation Error: Network connection dropped. Please check your internet and try again.".to_string()));
        }
        if lower.contains("device is mounted") || lower.contains("is mounted") || lower.contains("resource busy") || lower.contains("device or resource busy") {
            return Some((None, "Installation Error: The target drive is currently busy. Please reboot and try again.".to_string()));
        }
        if lower.contains("bootupd is required") {
            return Some((None, "Installation Error: Missing bootloader components (bootupd).".to_string()));
        }
        
        // Fallback error cleaner
        if let Some(idx) = lower.find("error:") {
            let clean = trimmed[idx..].to_string();
            let mut chars = clean.chars();
            let cap = match chars.next() {
                None => String::new(),
                Some(f) => f.to_uppercase().collect::<String>() + chars.as_str(),
            };
            return Some((None, format!("Installation Failed: {}", cap)));
        }
        return Some((None, format!("Installation Failed: {}", trimmed)));
    }

    // Pass through anything that isn't matched
    Some((extracted_pct, trimmed.to_string()))
}
