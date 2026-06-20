use libadwaita::prelude::*;
use libadwaita::{
    ActionRow, Application, ApplicationWindow, ComboRow, HeaderBar, PasswordEntryRow,
    PreferencesGroup,
};
use gtk::{
    Box, Button, CheckButton, Image, Label, MenuButton, Orientation, Popover,
    ProgressBar, ScrolledWindow, Spinner, Stack, StackTransitionType, Switch, TextView,
    gio,
};
use std::cell::{Cell, RefCell};
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
        state >= 60 // 60=CONNECTED_SITE, 70=CONNECTED_GLOBAL
    }
}

// Build number injected by CI as ALGA_BUILD_NUMBER env var (e.g. "70").
// Falls back to "dev" for local builds.
const ALGA_VERSION: &str = match option_env!("ALGA_BUILD_NUMBER") {
    Some(n) => n,
    None => "dev",
};

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

        // Tags are v{run_number} (e.g. "v70"); compare against installed version.
        // /var/lib/alga/current stores {"version":"v70",...} after a self-update.
        // If that file doesn't exist, fall back to ALGA_VERSION baked in at build time.
        let current_tag = std::fs::read_to_string("/var/lib/alga/current")
            .ok()
            .and_then(|s| {
                s.split("\"version\":\"")
                    .nth(1)
                    .and_then(|s| s.split('\"').next())
                    .map(|s| s.to_string())
            })
            .unwrap_or_else(|| format!("v{}", ALGA_VERSION));

        let is_newer = tag != current_tag.as_str();

        if is_newer {
            Ok(Some(tag.to_string()))
        } else {
            Ok(None)
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

        let run_number = version.trim_start_matches('v');
        let asset_name = format!("alga-1.0.0-{}-x86_64.pkg.tar.zst", run_number);
        let url = format!(
            "https://github.com/zamkara/alga/releases/download/{}/{}",
            version, asset_name
        );
        let resp = client.get(&url).send().await.map_err(|e| format!("Download error: {}", e))?;

        if !resp.status().is_success() {
            return Err(format!("Download returned {}", resp.status()));
        }

        let bytes = resp.bytes().await.map_err(|e| format!("Read error: {}", e))?;

        // Download to /tmp as current user, then install via pkexec
        let tmp_pkg = "/tmp/alga-update.pkg.tar.zst";
        std::fs::write(tmp_pkg, &bytes).map_err(|e| format!("Write error: {}", e))?;

        let now = chrono_now();
        let install_script = format!(
            r#"set -e
mkdir -p /var/lib/alga/bin
mkdir -p /tmp/alga-extract
tar -I zstd -xf {tmp_pkg} -C /tmp/alga-extract
mv /tmp/alga-extract/usr/bin/alga /var/lib/alga/bin/alga
chmod 755 /var/lib/alga/bin/alga
rm -rf /tmp/alga-extract {tmp_pkg}
printf '{{"version":"{ver}","updated_at":"{ts}"}}' > /var/lib/alga/current"#,
            tmp_pkg = tmp_pkg,
            ver = version,
            ts = now,
        );

        let output = tokio::process::Command::new("pkexec")
            .args(["bash", "-c", &install_script])
            .output()
            .await
            .map_err(|e| format!("pkexec error: {}", e))?;

        let _ = std::fs::remove_file(tmp_pkg);

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!("Install failed: {}", stderr));
        }

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

ESP="${ESP:-}"
if [ -z "$ESP" ]; then
    # Cari EFI System Partition
    for candidate in "/boot" "/efi" "/boot/efi"; do
        if mountpoint -q "$candidate" 2>/dev/null && df -T "$candidate" 2>/dev/null | grep -q vfat; then
            ESP="$candidate"
            break
        fi
    done
fi
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

# Remove auto-generated bootc/ostree dirs (format: default-<hash>) — these are written
# by 'bootc install' and never cleaned up; they waste ESP space.
for _rmdir in "$ESP/ostree/default-"*/; do
    [ -d "$_rmdir" ] || continue
    echo "bls-sync: Removing auto-generated dir $(basename "$_rmdir")"
    rm -rf "$_rmdir" 2>/dev/null || true
done

# Prune ESP: keep only the 2 most recent deployment dirs to prevent ESP from filling up.
_esp_n=0
for _esp_pd in $(ls -dt "$ESP/ostree"/[0-9a-f]*.?/ 2>/dev/null); do
    _esp_n=$((_esp_n + 1))
    if [ "$_esp_n" -le 2 ]; then continue; fi
    _esp_pi=$(basename "$_esp_pd")
    echo "bls-sync: Pruning old ESP deployment: $_esp_pi"
    rm -f "$ESP/loader/entries/ostree-$_esp_pi.conf" 2>/dev/null || true
    rm -rf "$_esp_pd" 2>/dev/null || true
done

# Mount /sysroot RW jika perlu — coba via device agar tidak gagal pada bind mount
if ! touch "$SYSROOT/.ark-bls-check" 2>/dev/null; then
    SYSROOT_DEV=$(findmnt -n -o SOURCE "$SYSROOT" 2>/dev/null || true)
    if [ -n "$SYSROOT_DEV" ]; then
        mount -o remount,rw "$SYSROOT_DEV" "$SYSROOT" 2>/dev/null || \
        mount -o remount,rw "$SYSROOT" 2>/dev/null || true
    else
        mount -o remount,rw "$SYSROOT" 2>/dev/null || true
    fi
fi
rm -f "$SYSROOT/.ark-bls-check" 2>/dev/null || true

export OSTREE_SYSROOT="$OSTREE_REPO"

deployments=$(ls -d "$DEPLOY_BASE"/*/ 2>/dev/null | xargs -n1 basename 2>/dev/null || true)

if [ -z "$deployments" ]; then
    deployments=$(ostree admin --sysroot="$SYSROOT" status 2>/dev/null | grep -oP 'ostree/deploy/default/deploy/\K[^ ]+' || true)
fi

if [ -z "$deployments" ]; then
    echo "bls-sync: No deployments found"
    exit 0
fi

staged_file="$SYSROOT/ostree/deploy/default/staged-deployment"
if [ -f "$staged_file" ]; then
    staged_id=$(grep -oP '"checksum"\s*:\s*"\K[a-f0-9]+' "$staged_file" 2>/dev/null | head -1 || true)
    staged_serial=$(grep -oP '"deployserial"\s*:\s*\K[0-9]+' "$staged_file" 2>/dev/null | head -1 || true)
    if [ -n "$staged_id" ]; then
        staged_serial="${staged_serial:-0}"
        full_staged="${staged_id}.${staged_serial}"
        if [ -d "$DEPLOY_BASE/$full_staged" ] && ! printf '%s' "$deployments" | grep -qF "$full_staged"; then
            deployments="$deployments
$full_staged"
            echo "bls-sync: Including staged deployment $full_staged"
        fi
    fi
fi

echo "bls-sync: Known deployments: $(printf '%s' "$deployments" | tr '\n' ' ')"

mkdir -p "$ESP/loader/entries" "$ESP/ostree"

ROOT_UUID=$(findmnt -n -o UUID "$SYSROOT" 2>/dev/null || blkid -s UUID -o value "$(findmnt -n -o SOURCE "$SYSROOT" 2>/dev/null)" 2>/dev/null || echo "")
ROOT_SUBVOL=$(findmnt -n -o OPTIONS "$SYSROOT" 2>/dev/null | tr ',' '\n' | grep '^subvol=' | head -1 | sed 's|^subvol=||;s|^/||' || true)

# Detect LUKS-encrypted root
LUKS_UUID=""
_root_source=$(findmnt -n -o SOURCE "$SYSROOT" 2>/dev/null || true)
if echo "$_root_source" | grep -q "^/dev/mapper/"; then
    _luks_name="${_root_source%%\[*}"
    _luks_name="${_luks_name##*/}"
    _luks_backing=$(cryptsetup status "$_luks_name" 2>/dev/null | awk '/device:/ {print $2}' || true)
    if [ -n "$_luks_backing" ]; then
        LUKS_UUID=$(blkid -s UUID -o value "$_luks_backing" 2>/dev/null || true)
    fi
fi

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

    initramfs_src=""
    for candidate in \
        "$modules_dir/$kver/initramfs.img" \
        "$deploy_path/boot/initramfs-$kver.img" \
        "$deploy_path/boot/initramfs-linux.img" \
        "$deploy_path/boot/initramfs-$kver-fallback.img"; do
        [ -f "$candidate" ] && initramfs_src="$candidate" && break
    done

    initramfs_dst="$ESP/ostree/$deploy_id/initramfs-$kver.img"

    mkdir -p "$ESP/ostree/$deploy_id"

    if [ ! -f "$vmlinuz_dst" ] || [ "$vmlinuz_src" -nt "$vmlinuz_dst" ]; then
        cp -f "$vmlinuz_src" "$vmlinuz_dst" || { echo "bls-sync: Failed to copy vmlinuz for $deploy_id, skipping"; continue; }
    fi

    if [ -f "$initramfs_src" ]; then
        if [ ! -f "$initramfs_dst" ] || [ "$initramfs_src" -nt "$initramfs_dst" ]; then
            cp -f "$initramfs_src" "$initramfs_dst" || { echo "bls-sync: Failed to copy initramfs for $deploy_id, skipping"; continue; }
        fi
    else
        if command -v dracut >/dev/null 2>&1; then
            dracut --force --no-hostonly --kver "$kver" \
                --kernel-image "$vmlinuz_dst" \
                "$initramfs_dst" 2>/dev/null || true
        elif command -v mkinitcpio >/dev/null 2>&1; then
            if [ -f "$deploy_path/etc/mkinitcpio.conf" ]; then
                cp "$deploy_path/etc/mkinitcpio.conf" /etc/mkinitcpio.conf.bls-tmp 2>/dev/null || true
            fi
            cp "$vmlinuz_src" "/boot/vmlinuz-$kver" 2>/dev/null || true
            mkinitcpio -k "$kver" -g "$initramfs_dst" 2>/dev/null || true
            rm -f "/boot/vmlinuz-$kver" 2>/dev/null || true
        fi
    fi

    [ ! -f "$initramfs_dst" ] && continue

    bootcsum="${deploy_id%.*}"
    bootserial="${deploy_id##*.}"
    boot_slot=""
    for slot in boot.0 boot.1; do
        if [ -L "$SYSROOT/ostree/$slot/default/$bootcsum/$bootserial" ] || \
           [ -d "$SYSROOT/ostree/$slot/default/$bootcsum/$bootserial" ]; then
            boot_slot="$slot"
            break
        fi
    done
    if [ -z "$boot_slot" ]; then
        boot_slot="boot.0"
        bootlink_dir="$SYSROOT/ostree/boot.0/default/$bootcsum"
        mkdir -p "$bootlink_dir" 2>/dev/null || true
        ln -sfn "../../../deploy/default/deploy/$deploy_id" "$bootlink_dir/$bootserial" 2>/dev/null || true
    fi
    ostree_param="ostree=/ostree/$boot_slot/default/${bootcsum}/${bootserial}"
    deploy_date=$(date -r "$deploy_path" "+%Y%m%d%H%M%S" 2>/dev/null || date "+%Y%m%d%H%M%S")
    title="Arch Linux $deploy_date"

    cmdline=""
    for cmdline_file in "$deploy_path/usr/lib/ostree-boot/cmdline" "$deploy_path/etc/kernel/cmdline"; do
        if [ -f "$cmdline_file" ]; then
            cmdline=$(tr '\n' ' ' < "$cmdline_file")
            break
        fi
    done
    if [ -z "$cmdline" ]; then
        if [ -n "$LUKS_UUID" ]; then
            if [ -n "$ROOT_SUBVOL" ] && [ "$ROOT_SUBVOL" != "/" ]; then
                cmdline="rd.luks.name=$LUKS_UUID=ark-root root=/dev/mapper/ark-root rootflags=subvol=$ROOT_SUBVOL rw quiet splash loglevel=3 rd.udev.log_priority=3"
            else
                cmdline="rd.luks.name=$LUKS_UUID=ark-root root=/dev/mapper/ark-root rw quiet splash loglevel=3 rd.udev.log_priority=3"
            fi
        elif [ -n "$ROOT_SUBVOL" ] && [ "$ROOT_SUBVOL" != "/" ]; then
            cmdline="root=UUID=$ROOT_UUID rootflags=subvol=$ROOT_SUBVOL rw quiet splash loglevel=3 rd.udev.log_priority=3"
        else
            cmdline="root=UUID=$ROOT_UUID rw quiet splash loglevel=3 rd.udev.log_priority=3"
        fi
    fi
    cmdline="$cmdline $ostree_param"

    entry_file="$ESP/loader/entries/ostree-$deploy_id.conf"
    if ! cat > "$entry_file" <<BLSENTRY
## This is a boot loader entry for ostree based on Ark Linux
title $title
version $kver
options $cmdline
linux /ostree/$deploy_id/vmlinuz-$kver
initrd /ostree/$deploy_id/initramfs-$kver.img
BLSENTRY
    then
        echo "bls-sync: Failed to write entry for $deploy_id (disk full?)"
        rm -f "$entry_file" 2>/dev/null || true
        continue
    fi

    echo "bls-sync: Generated entry for deployment $deploy_id (kernel $kver)"
done

for entry in "$ESP/loader/entries/ostree-"*.conf; do
    [ ! -f "$entry" ] && continue
    id=$(basename "$entry" .conf | sed 's/^ostree-//')
    found=0
    for d in $deployments; do
        d=$(echo "$d" | tr -d '\n\r ')
        [ "$id" = "$d" ] && found=1 && break
    done
    if [ "$found" = "0" ]; then
        echo "bls-sync: Removing stale entry $id"
        rm -f "$entry"
        rm -rf "$ESP/ostree/$id" 2>/dev/null || true
    fi
done

# Hapus semua entry yang title-nya mengandung "(ostree:" — format auto-generated bootc/ostree
for entry in "$ESP/loader/entries/"*.conf; do
    [ ! -f "$entry" ] && continue
    if grep -q "title.*ostree:" "$entry" 2>/dev/null; then
        echo "bls-sync: Removing auto-generated entry $(basename $entry)"
        rm -f "$entry"
    fi
done

if [ ! -f "$ESP/loader/loader.conf" ]; then
    cat > "$ESP/loader/loader.conf" <<LOADER
timeout 3
console-mode max
default @
LOADER
fi

mount -o remount,ro "$SYSROOT" 2>/dev/null || true
"#;



fn build_network_page(sender: std::sync::mpsc::Sender<String>) -> (Box, Rc<dyn Fn()>) {
    let wrapper = Box::new(Orientation::Vertical, 0);

    // ── Checking state (shown first) ──
    let checking_box = Box::new(Orientation::Vertical, 18);
    checking_box.set_margin_top(24);
    checking_box.set_margin_bottom(24);
    checking_box.set_margin_start(24);
    checking_box.set_margin_end(24);
    checking_box.set_vexpand(true);
    checking_box.set_valign(gtk::Align::Center);

    let spinner = Spinner::builder()
        .spinning(true)
        .halign(gtk::Align::Center)
        .margin_bottom(24)
        .build();
    spinner.set_size_request(64, 64);

    let checking_label = Label::builder()
        .label("Checking connection...")
        .halign(gtk::Align::Center)
        .build();
    checking_label.add_css_class("title-2");

    checking_box.append(&spinner);
    checking_box.append(&checking_label);

    // ── Offline state (shown only if not connected) ──
    let offline_box = Box::new(Orientation::Vertical, 18);
    offline_box.set_margin_top(24);
    offline_box.set_margin_bottom(24);
    offline_box.set_margin_start(24);
    offline_box.set_margin_end(24);
    offline_box.set_vexpand(true);
    offline_box.set_valign(gtk::Align::Center);
    offline_box.set_visible(false);

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

    offline_box.append(&offline_icon);
    offline_box.append(&msg_label);
    offline_box.append(&sub_label);

    // ── Footer (hidden during checking) ──
    let footer = Box::new(Orientation::Horizontal, 0);
    footer.set_margin_top(16);
    footer.set_margin_bottom(24);
    footer.set_margin_start(24);
    footer.set_margin_end(24);
    footer.set_visible(false);

    let settings_btn = Button::builder()
        .label("Open Network Settings")
        .hexpand(true)
        .css_classes(["suggested-action"])
        .build();
    footer.append(&settings_btn);

    // ── Assemble ──
    let content_box = Box::new(Orientation::Vertical, 0);
    content_box.append(&checking_box);
    content_box.append(&offline_box);
    content_box.append(&footer);
    wrapper.append(&content_box);

    settings_btn.connect_clicked(|_| {
        let _ = std::process::Command::new("gnome-control-center")
            .arg("wifi")
            .spawn();
    });

    // Initial check after a short delay so the spinner is visible first
    let check_sender = sender.clone();
    glib::timeout_add_local_once(std::time::Duration::from_millis(600), clone!(@weak checking_box, @weak offline_box, @weak footer, @weak spinner => move || {
        if nm::is_online() {
            let _ = check_sender.send("connected".to_string());
        } else {
            spinner.set_spinning(false);
            checking_box.set_visible(false);
            offline_box.set_visible(true);
            footer.set_visible(true);
        }
    }));

    // Re-check every 3s when offline to auto-advance when user connects
    let check_sender2 = sender.clone();
    glib::timeout_add_local(std::time::Duration::from_secs(3), move || {
        if nm::is_online() {
            let _ = check_sender2.send("connected".to_string());
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
            .application_id("com.zamkara.alga")
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

fn prefs_path() -> std::path::PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
    std::path::PathBuf::from(home).join(".config").join("alga").join("prefs.json")
}

fn load_prefs() -> (String, String) {
    let path = prefs_path();
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let app_interval = content.split("\"app_update_interval\":\"")
        .nth(1).and_then(|s| s.split('"').next()).unwrap_or("1d").to_string();
    let os_interval = content.split("\"os_update_interval\":\"")
        .nth(1).and_then(|s| s.split('"').next()).unwrap_or("1d").to_string();
    (app_interval, os_interval)
}

fn save_prefs(app_interval: &str, os_interval: &str) {
    let path = prefs_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let content = format!(
        "{{\"app_update_interval\":\"{}\",\"os_update_interval\":\"{}\"}}",
        app_interval, os_interval
    );
    let _ = std::fs::write(&path, content);
}

fn interval_to_seconds(s: &str) -> u32 {
    match s {
        "3h" => 3 * 3600,
        "1d" => 86400,
        "3d" => 3 * 86400,
        "1w" => 7 * 86400,
        "1mo" => 30 * 86400,
        _ => 86400,
    }
}

const INTERVAL_KEYS: &[&str] = &["3h", "1d", "3d", "1w", "1mo"];
const INTERVAL_LABELS: &[&str] = &["Every 3 hours", "Every day", "Every 3 days", "Every week", "Every month"];

fn interval_index(key: &str) -> u32 {
    INTERVAL_KEYS.iter().position(|&k| k == key).unwrap_or(1) as u32
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
        .default_height(660)
        .build();

    gtk::Window::set_default_icon_name("com.zamkara.alga");

    let main_box = Box::new(Orientation::Vertical, 0);
    let header_bar = HeaderBar::new();
    
    let back_btn = Button::builder()
        .icon_name("go-previous-symbolic")
        .visible(false)
        .build();
    back_btn.add_css_class("flat");
    header_bar.pack_start(&back_btn);
    
    // --- Popover and Menu Button ---
    let menu_btn = MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .build();
    menu_btn.add_css_class("flat");
    header_bar.pack_end(&menu_btn);

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

    // --- Page 1: System Update ---
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

    stack.add_named(&page1_box, Some("page1"));

    // --- Page 2: About App ---
    let page3_box = Box::new(Orientation::Vertical, 0);
    let content_box3 = Box::new(Orientation::Vertical, 12);
    content_box3.set_margin_top(32);
    content_box3.set_margin_bottom(24);
    content_box3.set_margin_start(32);
    content_box3.set_margin_end(32);
    content_box3.set_vexpand(true);

    let about_icon = Image::builder()
        .file("/usr/share/icons/hicolor/scalable/apps/com.zamkara.alga.svg")
        .pixel_size(96)
        .halign(gtk::Align::Center)
        .margin_bottom(12)
        .build();
    content_box3.append(&about_icon);

    let about_title = Label::builder()
        .label("Ark Wizard")
        .css_classes(vec!["title-1".to_string()])
        .halign(gtk::Align::Center)
        .build();
    content_box3.append(&about_title);

    let about_ver = Label::builder()
        .label(&format!("Version v{}", ALGA_VERSION))
        .css_classes(vec!["caption".to_string()])
        .halign(gtk::Align::Center)
        .build();
    content_box3.append(&about_ver);

    let about_desc = Label::builder()
        .label("Atomic deployment and update gateway to immutable Arch Linux built using Rust and GTK4/Libadwaita")
        .css_classes(vec!["monospace".to_string()])
        .justify(gtk::Justification::Center)
        .wrap(true)
        .halign(gtk::Align::Center)
        .margin_bottom(18)
        .build();
    content_box3.append(&about_desc);

    let pref_group = PreferencesGroup::builder()
        .build();

    let dev_row = ActionRow::builder()
        .title("Maintainer")
        .subtitle("zamkara")
        .build();
    pref_group.add(&dev_row);

    let website_row = ActionRow::builder()
        .title("Website")
        .subtitle("github.com/zamkara/alga")
        .activatable(true)
        .build();
    let link_icon = Image::builder()
        .icon_name("window-new-symbolic")
        .build();
    website_row.add_suffix(&link_icon);
    website_row.connect_activated(move |_| {
        let _ = std::process::Command::new("xdg-open")
            .arg("https://github.com/zamkara/alga")
            .spawn();
    });
    pref_group.add(&website_row);

    let license_row = ActionRow::builder()
        .title("License")
        .subtitle("GPL-3.0-only")
        .build();
    pref_group.add(&license_row);

    content_box3.append(&pref_group);

    let footer3 = Box::new(Orientation::Horizontal, 0);
    footer3.set_margin_top(16);
    footer3.set_margin_bottom(24);
    footer3.set_margin_start(24);
    footer3.set_margin_end(24);

    let alga_check_btn = Button::builder()
        .label("Sync Update")
        .css_classes(vec!["suggested-action".to_string()])
        .hexpand(true)
        .build();
    footer3.append(&alga_check_btn);

    page3_box.append(&content_box3);
    page3_box.append(&footer3);
    stack.add_named(&page3_box, Some("page_about"));

    // --- Page: Preferences ---
    let (init_app_interval, init_os_interval) = load_prefs();

    let prefs_page_box = Box::new(Orientation::Vertical, 0);
    let prefs_content = Box::new(Orientation::Vertical, 16);
    prefs_content.set_margin_top(24);
    prefs_content.set_margin_bottom(24);
    prefs_content.set_margin_start(24);
    prefs_content.set_margin_end(24);
    prefs_content.set_vexpand(true);

    let prefs_group = PreferencesGroup::builder()
        .title("Update Intervals")
        .description("How often Alga checks for updates in the background")
        .build();

    let interval_model = gtk::StringList::new(INTERVAL_LABELS);

    let app_interval_row = ComboRow::builder()
        .title("App Update Interval")
        .model(&interval_model)
        .selected(interval_index(&init_app_interval))
        .build();
    prefs_group.add(&app_interval_row);

    let os_interval_row = ComboRow::builder()
        .title("OS Update Interval")
        .model(&interval_model)
        .selected(interval_index(&init_os_interval))
        .build();
    prefs_group.add(&os_interval_row);

    prefs_content.append(&prefs_group);
    prefs_page_box.append(&prefs_content);
    stack.add_named(&prefs_page_box, Some("page_preferences"));

    // Save prefs when either combo changes
    app_interval_row.connect_selected_notify(clone!(@weak os_interval_row => move |row| {
        let app_key = INTERVAL_KEYS[row.selected() as usize];
        let os_key = INTERVAL_KEYS[os_interval_row.selected() as usize];
        save_prefs(app_key, os_key);
    }));
    os_interval_row.connect_selected_notify(clone!(@weak app_interval_row => move |row| {
        let app_key = INTERVAL_KEYS[app_interval_row.selected() as usize];
        let os_key = INTERVAL_KEYS[row.selected() as usize];
        save_prefs(app_key, os_key);
    }));

    // --- Page: Done (reused for system upgrade + alga self-update) ---
    let page_done_box = Box::new(Orientation::Vertical, 0);
    let content_done = Box::new(Orientation::Vertical, 18);
    content_done.set_margin_top(32);
    content_done.set_margin_bottom(32);
    content_done.set_margin_start(32);
    content_done.set_margin_end(32);
    content_done.set_vexpand(true);
    content_done.set_valign(gtk::Align::Center);

    let done_icon = Image::builder()
        .file("/usr/share/alga/ready-to-go.svg")
        .pixel_size(128)
        .halign(gtk::Align::Center)
        .margin_bottom(12)
        .build();
    let done_title = Label::builder()
        .label("")
        .css_classes(vec!["title-1".to_string()])
        .halign(gtk::Align::Center)
        .build();
    let done_desc = Label::builder()
        .label("")
        .justify(gtk::Justification::Center)
        .wrap(true)
        .halign(gtk::Align::Center)
        .build();
    content_done.append(&done_icon);
    content_done.append(&done_title);
    content_done.append(&done_desc);
    page_done_box.append(&content_done);

    let footer_done = Box::new(Orientation::Horizontal, 0);
    footer_done.set_margin_top(16);
    footer_done.set_margin_bottom(24);
    footer_done.set_margin_start(24);
    footer_done.set_margin_end(24);
    let done_btn = Button::builder()
        .label("")
        .css_classes(vec!["suggested-action".to_string()])
        .hexpand(true)
        .build();
    footer_done.append(&done_btn);
    page_done_box.append(&footer_done);
    stack.add_named(&page_done_box, Some("page_done"));

    // done_btn action: 0=reboot, 1=restart_alga
    let done_action: Rc<Cell<u8>> = Rc::new(Cell::new(0));
    done_btn.connect_clicked(clone!(@strong done_action => move |_| {
        if done_action.get() == 1 {
            restart_alga();
        } else {
            let _ = std::process::Command::new("systemctl").arg("reboot").spawn();
        }
    }));

    main_box.append(&stack);
    window.set_content(Some(&main_box));
    window.present();

    // --- Menu Popover Menu Items ---
    let popover = Popover::new();
    let menu_vbox = Box::new(Orientation::Vertical, 2);
    menu_vbox.set_margin_top(4);
    menu_vbox.set_margin_bottom(4);
    menu_vbox.set_margin_start(4);
    menu_vbox.set_margin_end(4);

    let menu_prefs_btn = Button::builder()
        .label("Preferences")
        .css_classes(vec!["flat".to_string()])
        .build();

    let menu_about_btn = Button::builder()
        .label("About App")
        .css_classes(vec!["flat".to_string()])
        .build();

    menu_vbox.append(&menu_prefs_btn);
    menu_vbox.append(&menu_about_btn);
    popover.set_child(Some(&menu_vbox));
    menu_btn.set_popover(Some(&popover));

    // --- Navigation Logic ---
    stack.connect_visible_child_notify(clone!(@weak window, @weak back_btn, @weak menu_btn => move |s| {
        let current = s.visible_child_name().unwrap_or_default().to_string();
        let show_back = current == "page_about" || current == "page_preferences";
        back_btn.set_visible(show_back);
        menu_btn.set_visible(!show_back);
        match current.as_str() {
            "page_about" => window.set_title(Some("About App")),
            "page_preferences" => window.set_title(Some("Preferences")),
            _ => window.set_title(Some("Software Updater")),
        }
    }));

    back_btn.connect_clicked(clone!(@weak stack => move |_| {
        stack.set_visible_child_name("page1");
    }));

    menu_prefs_btn.connect_clicked(clone!(@weak stack, @weak popover => move |_| {
        popover.popdown();
        stack.set_visible_child_name("page_preferences");
    }));

    menu_about_btn.connect_clicked(clone!(@weak stack, @weak popover => move |_| {
        popover.popdown();
        stack.set_visible_child_name("page_about");
    }));

    // --- State and Handlers for System Updater ---
    let state: Rc<RefCell<u8>> = Rc::new(RefCell::new(0));

    action_btn.connect_clicked(clone!(@weak action_btn, @weak progress_bar, @weak text_view, @weak scrolled, @weak desc, @weak icon, @weak stack, @weak done_title, @weak done_desc, @weak done_btn, @strong state, @strong done_action => move |_| {
        let s = *state.borrow();

        if s == 5 {
            let _ = std::process::Command::new("sudo").arg("reboot").status();
            return;
        }

        if s == 0 || s == 3 {
            *state.borrow_mut() = 99;
            action_btn.set_sensitive(false);
            action_btn.set_label("Checking...");
            desc.set_label("Checking for available system updates...");

            let (sender, receiver) = std::sync::mpsc::channel::<String>();

            std::thread::spawn(move || {
                // Sync BLS entries first so bootc can find the current deployment
                let _ = std::process::Command::new("pkexec")
                    .args(["bash", "-c", BLS_SYNC_SCRIPT])
                    .output();

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
                            let err = format!("{}{}", stdout, stderr).trim().to_string();
                            let _ = sender.send(format!("CHECK_FAILED:{}", err));
                        }
                    }
                    Err(e) => {
                        let _ = sender.send(format!("CHECK_FAILED:{}", e));
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
                        _ if msg.starts_with("CHECK_FAILED:") => {
                            *state.borrow_mut() = 3;
                            action_btn.set_label("Retry");
                            action_btn.set_sensitive(true);
                            let err = msg.trim_start_matches("CHECK_FAILED:").trim();
                            let detail = if err.is_empty() { "Unknown error.".to_string() } else { err.chars().take(200).collect::<String>() };
                            desc.set_label(&format!("Update check failed:\n{}", detail));
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

            let updating = Rc::new(Cell::new(true));
            glib::timeout_add_local(std::time::Duration::from_millis(150), clone!(@weak progress_bar, @strong updating => @default-return glib::ControlFlow::Break, move || {
                if !updating.get() {
                    return glib::ControlFlow::Break;
                }
                let frac = progress_bar.fraction();
                if frac < 0.85 {
                    progress_bar.set_fraction(frac + 0.003);
                }
                glib::ControlFlow::Continue
            }));

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

            glib::idle_add_local(clone!(@weak text_view, @weak progress_bar, @weak action_btn, @weak desc, @weak icon, @weak stack, @weak done_title, @weak done_desc, @weak done_btn, @strong state, @strong updating, @strong done_action => @default-return glib::ControlFlow::Continue, move || {
                while let Ok(text) = receiver.try_recv() {
                    if text == "EOF_SUCCESS" {
                        updating.set(false);
                        *state.borrow_mut() = 5;
                        done_action.set(0);
                        done_title.set_label("System Updated!");
                        done_desc.set_label("Reboot to apply the new system update.");
                        done_btn.set_label("Reboot Now");
                        log_to_desktop("[upgrade] EOF_SUCCESS: update completed.");
                        stack.set_visible_child_name("page_done");
                        return glib::ControlFlow::Break;
                    } else if text == "EOF_ERROR" {
                        updating.set(false);
                        *state.borrow_mut() = 6;
                        progress_bar.set_fraction(1.0);
                        action_btn.set_label("Update Failed");
                        action_btn.set_sensitive(true);
                        desc.set_label("Update encountered an error. Check the log for details.");
                        log_to_desktop("[upgrade] EOF_ERROR: update failed.");
                        return glib::ControlFlow::Break;
                    }

                    // Smooth progress mapping based on typical bootc upgrade phase output logs
                    let lower_text = text.to_lowercase();
                    let current_frac = progress_bar.fraction();
                    let mut target_frac = current_frac;

                    if lower_text.contains("pulling") || lower_text.contains("receiving") || lower_text.contains("downloading") {
                        if current_frac < 0.2 { target_frac = 0.2; }
                    } else if lower_text.contains("preparing") || lower_text.contains("checking") {
                        if current_frac < 0.4 { target_frac = 0.4; }
                    } else if lower_text.contains("writing") || lower_text.contains("copying") {
                        if current_frac < 0.6 { target_frac = 0.6; }
                    } else if lower_text.contains("staging") || lower_text.contains("staged") {
                        if current_frac < 0.8 { target_frac = 0.8; }
                    } else if lower_text.contains("synchronizing") || lower_text.contains("bootloader") {
                        if current_frac < 0.9 { target_frac = 0.9; }
                    }

                    // Increment in smooth 10% (0.1) steps towards target or slightly tick forward
                    if target_frac > current_frac {
                        let step = (target_frac - current_frac).min(0.1);
                        progress_bar.set_fraction(current_frac + step);
                    } else if current_frac < 0.9 {
                        // Small micro-progress ticks (0.5%) per log line to keep movement alive
                        progress_bar.set_fraction(current_frac + 0.005);
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

    // --- State and Handlers for App Self-Updater ---
    let alga_update_ver: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));

    alga_check_btn.connect_clicked(clone!(@weak alga_check_btn, @weak about_ver, @weak stack, @weak done_title, @weak done_desc, @weak done_btn, @strong alga_update_ver, @strong done_action => move |_| {
        let pending = alga_update_ver.borrow().clone();
        if let Some(version) = pending {
            alga_check_btn.set_sensitive(false);
            about_ver.set_label(&format!("Downloading {}...", version));
            let (sender, receiver) = std::sync::mpsc::channel::<String>();
            let ver = version.clone();
            std::thread::spawn(move || {
                match download_alga_update(&ver) {
                    Ok(_) => { let _ = sender.send("DONE".to_string()); }
                    Err(e) => { let _ = sender.send(format!("ERROR:{}", e)); }
                }
            });
            glib::idle_add_local(clone!(@weak alga_check_btn, @weak about_ver, @weak stack, @weak done_title, @weak done_desc, @weak done_btn, @strong alga_update_ver, @strong done_action => @default-return glib::ControlFlow::Continue, move || {
                while let Ok(msg) = receiver.try_recv() {
                    if msg == "DONE" {
                        done_action.set(1);
                        done_title.set_label("Alga Updated!");
                        done_desc.set_label("Restart the app to use the new version.");
                        done_btn.set_label("Restart Alga");
                        stack.set_visible_child_name("page_done");
                    } else if let Some(err) = msg.strip_prefix("ERROR:") {
                        about_ver.set_label(&format!("Download failed: {}", err));
                        alga_check_btn.set_label("Retry");
                        alga_check_btn.set_sensitive(true);
                    }
                    return glib::ControlFlow::Break;
                }
                glib::ControlFlow::Continue
            }));
        } else {
            alga_check_btn.set_sensitive(false);
            about_ver.set_label("Checking for alga updates...");
            let (sender, receiver) = std::sync::mpsc::channel::<String>();
            std::thread::spawn(move || {
                match check_alga_update() {
                    Ok(Some(version)) => { let _ = sender.send(format!("AVAILABLE:{}", version)); }
                    Ok(None) => { let _ = sender.send("UP_TO_DATE".to_string()); }
                    Err(e) => { let _ = sender.send(format!("ERROR:{}", e)); }
                }
            });
            glib::idle_add_local(clone!(@weak alga_check_btn, @weak about_ver, @strong alga_update_ver => @default-return glib::ControlFlow::Continue, move || {
                while let Ok(msg) = receiver.try_recv() {
                    if msg == "UP_TO_DATE" {
                        about_ver.set_label(&format!("v{} (Already up to date)", ALGA_VERSION));
                        alga_check_btn.set_sensitive(true);
                    } else if let Some(ver) = msg.strip_prefix("AVAILABLE:") {
                        *alga_update_ver.borrow_mut() = Some(ver.to_string());
                        about_ver.set_label(&format!("Update available: {}", ver));
                        alga_check_btn.set_label("Update Alga");
                        alga_check_btn.set_sensitive(true);
                    } else if let Some(err) = msg.strip_prefix("ERROR:") {
                        about_ver.set_label(&format!("Check failed: {}", err));
                        alga_check_btn.set_sensitive(true);
                    }
                    return glib::ControlFlow::Break;
                }
                glib::ControlFlow::Continue
            }));
        }
    }));

    // --- Periodic background update checks ---
    {
        let (app_key, os_key) = load_prefs();
        let app_secs = interval_to_seconds(&app_key);
        let os_secs = interval_to_seconds(&os_key);

        // App update check — background thread, result delivered to main loop via mpsc
        let (app_tx, app_rx) = std::sync::mpsc::channel::<String>();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(app_secs as u64));
                if let Ok(Some(ver)) = check_alga_update() {
                    let _ = app_tx.send(ver);
                }
            }
        });
        let app_ref = app.clone();
        glib::timeout_add_seconds_local(1, move || {
            if let Ok(ver) = app_rx.try_recv() {
                let n = gio::Notification::new("App Update Available");
                n.set_body(Some(&format!("Alga {} is ready to install.", ver)));
                n.add_button("View Update", "app.show-app-update");
                n.add_button("Skip", "app.dismiss-notification");
                app_ref.send_notification(Some("alga-app-update"), &n);
            }
            glib::ControlFlow::Continue
        });

        // OS update check
        let (os_tx, os_rx) = std::sync::mpsc::channel::<()>();
        std::thread::spawn(move || {
            loop {
                std::thread::sleep(std::time::Duration::from_secs(os_secs as u64));
                let out = std::process::Command::new("pkexec")
                    .args(["bootc", "upgrade", "--check"])
                    .output();
                if let Ok(o) = out {
                    let combined = format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)).to_lowercase();
                    if o.status.success() && !combined.contains("no update") && !combined.contains("no changes") {
                        let _ = os_tx.send(());
                    }
                }
            }
        });
        let app_ref = app.clone();
        glib::timeout_add_seconds_local(1, move || {
            if os_rx.try_recv().is_ok() {
                let n = gio::Notification::new("System Update Available");
                n.set_body(Some("A new system update is ready to install."));
                n.add_button("View Update", "app.show-os-update");
                n.add_button("Skip", "app.dismiss-notification");
                app_ref.send_notification(Some("alga-os-update"), &n);
            }
            glib::ControlFlow::Continue
        });

        // Register actions for notification buttons
        let show_app_update = gio::SimpleAction::new("show-app-update", None);
        show_app_update.connect_activate(clone!(@weak stack, @weak window => move |_, _| {
            stack.set_visible_child_name("page_about");
            window.present();
        }));
        app.add_action(&show_app_update);

        let show_os_update = gio::SimpleAction::new("show-os-update", None);
        show_os_update.connect_activate(clone!(@weak stack, @weak window => move |_, _| {
            stack.set_visible_child_name("page1");
            window.present();
        }));
        app.add_action(&show_os_update);

        let dismiss = gio::SimpleAction::new("dismiss-notification", None);
        dismiss.connect_activate(move |_, _| {});
        app.add_action(&dismiss);
    }
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
        window, .window-frame,
        headerbar, .titlebar,
        scrolledwindow,
        listview, listview row,
        list, list row,
        preferencesgroup > box > box,
        entry, entry:focus,
        progressbar trough,
        progressbar progress { box-shadow: none; }
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

    gtk::Window::set_default_icon_name("com.zamkara.alga");

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
    let target_encryption = Rc::new(RefCell::new(false));
    let target_enc_mode = Rc::new(RefCell::new(String::from("passphrase")));
    let target_passphrase = Rc::new(RefCell::new(String::new()));
    let target_recovery_key = Rc::new(RefCell::new(String::new()));
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
        .args(["-d", "-n", "-P", "-b", "-o", "NAME,SIZE,MODEL,RM,TRAN,TYPE"])
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
                
                let size_bytes: u64 = extract_val(line, "SIZE").parse().unwrap_or(0);
                let size_display = format_bytes(size_bytes);
                let too_small = size_bytes < MIN_DISK_BYTES;
                let model = extract_val(line, "MODEL");

                let display_title = if model.is_empty() { format!("Unknown Device (/dev/{})", name) } else { model };
                let display_subtitle = if too_small {
                    format!("/dev/{} - {} — too small (min. 20 GB)", name, size_display)
                } else {
                    format!("/dev/{} - {}", name, size_display)
                };
                let machine_name = format!("/dev/{}", name);

                let row = ActionRow::builder().title(&display_title).subtitle(&display_subtitle).build();
                let check = CheckButton::builder().build();
                check.set_widget_name(&machine_name);
                check.set_sensitive(!too_small);

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

    // --- Encryption Page ---
    let page_enc_box = Box::new(Orientation::Vertical, 0);
    let content_enc = Box::new(Orientation::Vertical, 18);
    content_enc.set_margin_top(24);
    content_enc.set_margin_bottom(24);
    content_enc.set_margin_start(24);
    content_enc.set_margin_end(24);
    content_enc.set_vexpand(true);

    let title_enc = Label::builder()
        .label("<b>Disk Encryption</b>")
        .use_markup(true)
        .halign(gtk::Align::Start)
        .build();
    title_enc.add_css_class("title-2");

    // Group 1: Encryption toggle (mirip NVIDIA/GRUB switch)
    let grp_enc = PreferencesGroup::builder()
        .description("Protect your data with LUKS2 encryption. A passphrase is required at every boot unless TPM2 is used.")
        .build();
    let enc_switch = Switch::builder().active(false).valign(gtk::Align::Center).build();
    let row_enc = ActionRow::builder()
        .title("Encrypt Disk")
        .subtitle("Full-disk encryption (recommended)")
        .build();
    row_enc.add_suffix(&enc_switch);
    grp_enc.add(&row_enc);

    // Group 2: Passphrase entries
    let pass_entry = PasswordEntryRow::builder()
        .title("Passphrase")
        .build();
    let pass_confirm = PasswordEntryRow::builder()
        .title("Confirm Passphrase")
        .build();
    let strength_label = Label::builder()
        .halign(gtk::Align::Start)
        .margin_start(16)
        .margin_top(4)
        .margin_bottom(8)
        .build();

    let grp_pass = PreferencesGroup::new();
    grp_pass.add(&pass_entry);
    grp_pass.add(&pass_confirm);
    grp_pass.add(&strength_label);

    // Layout scrollable
    let scroll_box_enc = Box::new(Orientation::Vertical, 12);
    scroll_box_enc.set_margin_start(2);
    scroll_box_enc.set_margin_end(2);
    scroll_box_enc.append(&grp_enc);
    scroll_box_enc.append(&grp_pass);

    let scroll_enc = ScrolledWindow::builder()
        .child(&scroll_box_enc)
        .vexpand(true)
        .build();
    content_enc.append(&title_enc);
    content_enc.append(&scroll_enc);
    page_enc_box.append(&content_enc);

    let footer_enc = Box::new(Orientation::Horizontal, 0);
    footer_enc.set_margin_top(16);
    footer_enc.set_margin_bottom(24);
    footer_enc.set_margin_start(24);
    footer_enc.set_margin_end(24);
    let next_btn_enc = Button::builder()
        .label("Next")
        .css_classes(["suggested-action"])
        .hexpand(true)
        .build();
    footer_enc.append(&next_btn_enc);
    page_enc_box.append(&footer_enc);
    stack.add_named(&page_enc_box, Some("page_enc"));

    // --- Encryption Page Signals ---

    grp_pass.set_visible(enc_switch.is_active());
    enc_switch.connect_state_set(clone!(@strong grp_pass => @default-return glib::Propagation::Proceed, move |_, state| {
        grp_pass.set_visible(state);
        glib::Propagation::Proceed
    }));

    // Passphrase validation + strength indicator
    let validate_pass = clone!(@weak pass_entry, @weak pass_confirm, @weak strength_label, @weak next_btn_enc, @strong target_passphrase => move || {
        let pass = pass_entry.text().as_str().to_string();
        let confirm = pass_confirm.text().as_str().to_string();
        let mut valid = !pass.is_empty() && pass == confirm;
        *target_passphrase.borrow_mut() = pass.clone();

        if confirm.is_empty() {
            pass_confirm.remove_css_class("error");
        } else if valid {
            pass_confirm.remove_css_class("error");
        } else {
            pass_confirm.add_css_class("error");
            valid = false;
        }

        // Strength
        if !pass.is_empty() {
            let has_upper = pass.chars().any(|c| c.is_uppercase());
            let has_lower = pass.chars().any(|c| c.is_lowercase());
            let has_digit = pass.chars().any(|c| c.is_ascii_digit());
            let has_symbol = pass.chars().any(|c| !c.is_alphanumeric());
            let variety = [has_upper, has_lower, has_digit, has_symbol].into_iter().filter(|&x| x).count();
            let length = pass.len();

            if length < 8 || variety < 2 {
                strength_label.set_text("Weak — make it longer or more complex");
                strength_label.remove_css_class("success");
                strength_label.add_css_class("error");
            } else if length < 12 || variety < 3 {
                strength_label.set_text("Fair — consider making it longer");
                strength_label.remove_css_class("success");
                strength_label.remove_css_class("error");
                strength_label.add_css_class("warning");
            } else {
                strength_label.set_text("Strong passphrase");
                strength_label.remove_css_class("error");
                strength_label.remove_css_class("warning");
                strength_label.add_css_class("success");
            }
            strength_label.set_visible(true);
        } else {
            strength_label.set_visible(false);
        }

        next_btn_enc.set_sensitive(valid);
    });

    pass_entry.connect_changed(clone!(@strong validate_pass => move |_| {
        validate_pass();
    }));
    pass_confirm.connect_changed(clone!(@strong validate_pass => move |_| {
        validate_pass();
    }));

    // Next button: save state + advance
    next_btn_enc.connect_clicked(clone!(@weak stack, @strong target_encryption, @strong target_enc_mode, @strong target_passphrase, @weak enc_switch, @weak pass_entry => move |_| {
        let on = enc_switch.is_active();
        *target_encryption.borrow_mut() = on;
        *target_enc_mode.borrow_mut() = if on { "passphrase" } else { "" }.to_string();
        *target_passphrase.borrow_mut() = if on { pass_entry.text().as_str().to_string() } else { String::new() };
        stack.set_visible_child_name("page4");
    }));

    // --- Page 4: Detailed Confirmation ---
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
        back_btn.set_visible(current == "page2" || current == "page3" || current == "page_enc" || current == "page4");
    }));

    back_btn.connect_clicked(clone!(@weak stack => move |_| {
        let current = stack.visible_child_name().unwrap_or_default();
        if current == "page2" {
            stack.set_visible_child_name("page1");
        } else if current == "page3" {
            stack.set_visible_child_name("page1");
        } else if current == "page_enc" {
            stack.set_visible_child_name("page3");
        } else if current == "page4" {
            stack.set_visible_child_name("page_enc");
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
        
        stack.set_visible_child_name("page_enc");
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
        let orundum_tag = variant.split(':').last().unwrap_or("ark").to_string();
        let zram_val = target_zram.borrow().clone();
        let enc_on = target_encryption.borrow().clone();
        let enc_mode = target_enc_mode.borrow().clone();
        let pass = target_passphrase.borrow().clone();
        
        // Write passphrase to temp file for secure shell access
        let keyfile = if enc_on && !pass.is_empty() {
            let path = format!("/tmp/.ark-key-{}", std::process::id());
            let _ = std::fs::write(&path, &pass);
            path
        } else {
            String::new()
        };
        let use_tpm2_str = if enc_on && (enc_mode == "tpm2-only" || enc_mode == "tpm2-passphrase") {
            "yes"
        } else {
            ""
        };
        
        let (sender, receiver) = std::sync::mpsc::channel::<String>();
        let (kill_tx, mut kill_rx) = oneshot::channel::<()>();
        *cancel_sender.borrow_mut() = Some(kill_tx);
        
        glib::idle_add_local(clone!(@weak text_view, @weak progress_bar, @weak stack, @weak cancel_btn, @weak title4, @strong cancel_sender, @strong pulse_timeout, @strong target_recovery_key => @default-return glib::ControlFlow::Continue, move || {
            while let Ok(msg) = receiver.try_recv() {
                // Capture recovery key
                if let Some(key) = msg.strip_prefix("RECOVERY_KEY=") {
                    *target_recovery_key.borrow_mut() = key.to_string();
                }
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
                    "pkill -9 udisksd gvfs-udisks2-volume-monitor gvfsd 2>/dev/null || true; \
                     killall -9 bootc skopeo 2>/dev/null || true; \
                     grep -E '^{disk}' /proc/mounts | awk '{{print $2}}' | sort -r | \
                       while read _mp; do umount -f \"$_mp\" 2>/dev/null || umount -l \"$_mp\" 2>/dev/null || true; done; \
                     for p in {disk}*; do umount -l \"$p\" 2>/dev/null || true; done; \
                     umount -l /run/bootc/mounts/rootfs 2>/dev/null || true; \
                     umount -R /mnt 2>/dev/null || true; \
                     umount -l /dev/mapper/ark-root 2>/dev/null || true; \
                     for _dm in $(dmsetup ls 2>/dev/null | awk '{{print $1}}'); do \
                       _dm_dev=$(dmsetup info -c --noheadings -o blkdevs_used \"$_dm\" 2>/dev/null || true); \
                       if echo \"$_dm_dev\" | grep -q \"$(basename {disk})\"; then \
                         umount -l \"/dev/mapper/$_dm\" 2>/dev/null || true; \
                         cryptsetup close \"$_dm\" 2>/dev/null || dmsetup remove --force \"$_dm\" 2>/dev/null || true; \
                       fi; \
                     done; \
                     cryptsetup close ark-root 2>/dev/null || true; \
                     dmsetup remove --force ark-root 2>/dev/null || true; \
                     btrfs device scan --forget 2>/dev/null || true; \
                     wipefs -af {disk}* 2>/dev/null || true; \
                     for p in {disk}*; do dd if=/dev/zero of=\"$p\" bs=1M count=10 status=none 2>/dev/null || true; done; \
                     dd if=/dev/zero of={disk} bs=1M count=10 status=none 2>/dev/null || true; \
                     sync; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || true; \
                     sleep 2; \
                     DISK_BYTES=$(blockdev --getsize64 {disk} 2>/dev/null || echo 0) && \
                     [ \"$DISK_BYTES\" -lt 21474836480 ] && echo \"ERROR: Disk too small ($(( DISK_BYTES / 1024 / 1024 )) MB). Minimum 20 GB required.\" && exit 1; \
                     if echo '{disk}' | grep -qE 'nvme|mmcblk'; then EFI_PART='{disk}p1'; ROOT_PART='{disk}p2'; else EFI_PART='{disk}1'; ROOT_PART='{disk}2'; fi && \
                     printf 'label: gpt\\nsize=1024MiB, type=C12A7328-F81F-11D2-BA4B-00A0C93EC93B, name=EFI-SYSTEM\\ntype=0FC63DAF-8483-4772-8E79-3D69D8477DE4\\n' | sfdisk --wipe always --force {disk} && \
                     partprobe {disk} 2>/dev/null || blockdev --rereadpt {disk} 2>/dev/null || true && \
                     udevadm settle 2>/dev/null || true && \
                     for _i in 1 2 3 4 5 6 7 8 9 10; do \
                       test -b \"$EFI_PART\" && test -b \"$ROOT_PART\" && break; \
                       udevadm trigger --action=add --subsystem-match=block 2>/dev/null || true; \
                       udevadm settle 2>/dev/null || true; \
                       sleep 1; \
                     done && \
                     for _p in \"$EFI_PART\" \"$ROOT_PART\"; do \
                       if ! test -b \"$_p\"; then \
                         _n=$(basename \"$_p\"); \
                         _mm=$(awk -v n=\"$_n\" -v OFS=: '$4==n {{print $1,$2}}' /proc/partitions 2>/dev/null | head -1); \
                         if [ -n \"$_mm\" ]; then \
                           rm -f \"$_p\" 2>/dev/null || true; \
                           mknod \"$_p\" b \"${{_mm%%:*}}\" \"${{_mm##*:}}\" 2>/dev/null || true; \
                         fi; \
                       fi; \
                     done && \
                     test -b \"$EFI_PART\" || {{ \
                       echo \"ERROR: EFI partition $EFI_PART not found.\"; \
                       echo \"DEBUG proc: $(grep -E 'sda|nvme|mmcblk' /proc/partitions 2>/dev/null | tr '\\n' '|')\"; \
                       echo \"DEBUG sys: $(ls /sys/class/block/ 2>/dev/null | grep -E 'sda|nvme|mmcblk' | tr '\\n' '|')\"; \
                       echo \"DEBUG dev: $(ls /dev/ 2>/dev/null | grep -E 'sda|nvme|mmcblk' | tr '\\n' '|')\"; \
                       echo \"DEBUG fuser: $(fuser {disk}* 2>/dev/null | tr '\\n' '|')\"; \
                       exit 1; \
                     }} && \
                      test -b \"$ROOT_PART\" || {{ echo \"ERROR: Root partition $ROOT_PART not found.\"; exit 1; }} && \
                      PASSPHRASE=$([ -n \"{keyfile}\" ] && cat \"{keyfile}\" 2>/dev/null || true) && rm -f \"{keyfile}\" 2>/dev/null || true && \
                      ROOT_DEV=\"$ROOT_PART\" && \
                      if [ -n \"{keyfile}\" ] && [ -z \"$PASSPHRASE\" ]; then {{ echo \"ERROR: Failed to read passphrase from keyfile\"; exit 1; }}; fi && \
                      if [ -n \"$PASSPHRASE\" ]; then \
                        ENROLL_KEY=$(mktemp) && printf '%s' \"$PASSPHRASE\" > \"$ENROLL_KEY\" && \
                        pkill -9 udisksd gvfs-udisks2-volume-monitor 2>/dev/null || true && \
                        printf '%s' \"$PASSPHRASE\" | cryptsetup luksFormat --type luks2 --pbkdf argon2id --iter-time 2000 --key-file - \"$ROOT_PART\" || {{ echo \"ERROR: cryptsetup luksFormat failed\"; exit 1; }} && \
                        udevadm settle 2>/dev/null || true && \
                        _cs_err=$(printf '%s' \"$PASSPHRASE\" | cryptsetup open --key-file - \"$ROOT_PART\" ark-root 2>&1) || {{ echo \"ERROR: cryptsetup open failed: $_cs_err\"; exit 1; }} && \
                        ROOT_DEV=\"/dev/mapper/ark-root\" && \
                        if [ -n \"{use_tpm2}\" ]; then \
                          systemd-cryptenroll --unlock-key-file=\"$ENROLL_KEY\" --tpm2-device=auto --tpm2-pcrs=0+7 \"$ROOT_PART\"; \
                        fi && \
                        RECOVERY_KEY=$(systemd-cryptenroll --unlock-key-file=\"$ENROLL_KEY\" --recovery-key \"$ROOT_PART\" 2>&1 | grep -oE '[A-Z0-9]+-[A-Z0-9]+-[A-Z0-9]+-[A-Z0-9]+' || true) && \
                        rm -f \"$ENROLL_KEY\" && \
                        [ -n \"$RECOVERY_KEY\" ] && echo \"RECOVERY_KEY=$RECOVERY_KEY\" || true; \
                      fi && \
                      mkfs.vfat -F32 -n EFI-SYSTEM $EFI_PART && \
                      mkfs.btrfs -f -L root $ROOT_DEV && \
                      mount -t btrfs $ROOT_DEV /mnt && \
                      btrfs subvolume create /mnt/@ && \
                      btrfs subvolume create /mnt/@var && \
                      btrfs subvolume create /mnt/@var-log && \
                      btrfs subvolume create /mnt/@var-cache && \
                      btrfs subvolume create /mnt/@var-tmp && \
                      btrfs subvolume create /mnt/@tmp && \
                      btrfs subvolume create /mnt/@snapshots && \
                       btrfs subvolume create /mnt/@opt && \
                       btrfs subvolume create /mnt/@nix && \
                       btrfs subvolume create /mnt/@orundum && \
                       umount /mnt && \
                      mount -t btrfs -o subvol=@ $ROOT_DEV /mnt && \
                       mkdir -p /mnt/var /mnt/tmp /mnt/.snapshots /mnt/opt /mnt/nix /mnt/orundum && \
                       mount -t btrfs -o subvol=@var $ROOT_DEV /mnt/var && \
                      mkdir -p /mnt/var/log /mnt/var/cache /mnt/var/tmp && \
                      mount -t btrfs -o subvol=@var-log $ROOT_DEV /mnt/var/log && \
                      mount -t btrfs -o subvol=@var-cache $ROOT_DEV /mnt/var/cache && \
                      mount -t btrfs -o subvol=@var-tmp $ROOT_DEV /mnt/var/tmp && \
                      mount -t btrfs -o subvol=@tmp $ROOT_DEV /mnt/tmp && \
                      mount -t btrfs -o subvol=@snapshots $ROOT_DEV /mnt/.snapshots && \
                       mount -t btrfs -o subvol=@opt $ROOT_DEV /mnt/opt && \
                       mount -t btrfs -o subvol=@nix $ROOT_DEV /mnt/nix && \
                       mount -t btrfs -o subvol=@orundum $ROOT_DEV /mnt/orundum && \
                      _bootc_ok=0; for _try in 1 2 3; do \
                        bootc install to-filesystem --source-imgref docker://{variant} --bootloader none /mnt && {{ _bootc_ok=1; break; }}; \
                        echo \"bootc install attempt $_try/3 failed, retrying in 5s...\"; sleep 5; \
                      done; [ \"$_bootc_ok\" = 1 ] || {{ echo \"ERROR: bootc install failed after 3 attempts\"; exit 1; }} && \
                      mount $EFI_PART /mnt/boot && \
                      mkdir -p /tmp/rw_root && \
                      mount -t btrfs -o subvol=@ $ROOT_DEV /tmp/rw_root && \
                      DEPLOY_ETC=$(ls -d /tmp/rw_root/ostree/deploy/default/deploy/*/etc | head -n 1) && \
                      EFI_UUID=$(blkid -s UUID -o value $EFI_PART) && \
                      if [ \"$ROOT_DEV\" != \"$ROOT_PART\" ]; then \
                        LUKS_UUID=$(blkid -s UUID -o value \"$ROOT_PART\") && \
                        printf 'ark-root UUID=%s none luks,discard 0 0\\n' \"$LUKS_UUID\" >> $DEPLOY_ETC/crypttab && \
                        ROOT_FS_SPEC=\"/dev/mapper/ark-root\"; \
                      else \
                        ROOT_UUID=$(blkid -s UUID -o value \"$ROOT_PART\") && \
                        ROOT_FS_SPEC=\"UUID=$ROOT_UUID\"; \
                      fi && \
                      printf '%s /var         btrfs subvol=@var,compress=zstd,noatime 0 0\\n' \"$ROOT_FS_SPEC\" >> $DEPLOY_ETC/fstab && \
                      printf '%s /var/log     btrfs subvol=@var-log,compress=zstd,noatime 0 0\\n' \"$ROOT_FS_SPEC\" >> $DEPLOY_ETC/fstab && \
                      printf '%s /var/cache   btrfs subvol=@var-cache,compress=zstd,noatime 0 0\\n' \"$ROOT_FS_SPEC\" >> $DEPLOY_ETC/fstab && \
                      printf '%s /var/tmp     btrfs subvol=@var-tmp,compress=zstd,noatime 0 0\\n' \"$ROOT_FS_SPEC\" >> $DEPLOY_ETC/fstab && \
                      printf '%s /tmp         btrfs subvol=@tmp,compress=zstd,noatime 0 0\\n' \"$ROOT_FS_SPEC\" >> $DEPLOY_ETC/fstab && \
                      printf '%s /.snapshots  btrfs subvol=@snapshots,compress=zstd,noatime,nofail 0 0\\n' \"$ROOT_FS_SPEC\" >> $DEPLOY_ETC/fstab && \
                      printf '%s /opt         btrfs subvol=@opt,compress=zstd,noatime,nofail 0 0\\n' \"$ROOT_FS_SPEC\" >> $DEPLOY_ETC/fstab && \
                      printf '%s /nix         btrfs subvol=@nix,compress=zstd,noatime,nofail 0 0\\n' \"$ROOT_FS_SPEC\" >> $DEPLOY_ETC/fstab && \
                      printf '%s /orundum     btrfs subvol=@orundum,compress=zstd,noatime,nofail 0 0\\n' \"$ROOT_FS_SPEC\" >> $DEPLOY_ETC/fstab && \
                      printf 'UUID=%s /boot        vfat  umask=0077 0 2\\n' \"$EFI_UUID\" >> $DEPLOY_ETC/fstab && \
                      if [ \"$ROOT_DEV\" != \"$ROOT_PART\" ]; then \
                        mkdir -p /mnt/var/lib && \
                        cryptsetup luksHeaderBackup \"$ROOT_PART\" --header-backup-file /mnt/var/lib/luks-header.backup && \
                        chmod 600 /mnt/var/lib/luks-header.backup && \
                        printf '%s' \"$RECOVERY_KEY\" > /mnt/var/lib/recovery-key.txt 2>/dev/null || true; \
                      fi && \
                      DEPLOY_NIX_DIR=$(ls -d /tmp/rw_root/ostree/deploy/default/deploy/*/ 2>/dev/null | head -n 1) && \
                      if [ -n \"$DEPLOY_NIX_DIR\" ] && [ -d \"${{DEPLOY_NIX_DIR}}nix\" ]; then \
                        cp -a \"${{DEPLOY_NIX_DIR}}nix/.\" /mnt/nix/; \
                      fi && \
                      mkdir -p /mnt/nix/var && \
                      rm -rf /mnt/nix/var/nix && \
                      ln -sf /var/nix /mnt/nix/var/nix && \
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
                     echo '98% Setting up Arch container...' && \
                     mkdir -p /mnt/orundum/containers && \
                     podman --root /mnt/orundum/containers/storage --runroot /tmp/orundum-run --storage-driver overlay pull ghcr.io/zamkara/ark-orundum:{orundum_tag} 2>&1 || echo 'Arch container not pulled (no network), distrobox will pull on first use' && \
                     umount -l /tmp/rw_root && \
                      umount -l /mnt/boot && umount -l /mnt/nix && umount -l /mnt/orundum && umount -l /mnt/opt && umount -l /mnt/.snapshots && \
                     umount -l /mnt/tmp && umount -l /mnt/var/tmp && umount -l /mnt/var/cache && \
                     umount -l /mnt/var/log && umount -l /mnt/var && umount -l /mnt",
                     disk = disk,
                     variant = variant,
                     orundum_tag = orundum_tag,
                     zram = zram_val,
                     keyfile = keyfile,
                     use_tpm2 = use_tpm2_str
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
                              ROOT_PART=$(lsblk -rno PATH,FSTYPE {disk} | grep -i 'btrfs' | head -n1 | awk '{{print $1}}'); \
                              if [ -z \"$ROOT_PART\" ] && [ -b /dev/mapper/ark-root ]; then \
                                ROOT_PART=\"/dev/mapper/ark-root\"; \
                              fi && \
                              [ -z \"$EFI_PART\" ] && echo 'Error: EFI partition not found' && exit 1; \
                              [ -z \"$ROOT_PART\" ] && echo 'Error: Root partition not found' && exit 1; \
                              mkdir -p /tmp/root_mnt /tmp/efi_mnt; \
                              umount -l /tmp/root_mnt/boot/efi 2>/dev/null || true; \
                              umount -l /tmp/root_mnt/boot 2>/dev/null || true; \
                              umount -l /tmp/root_mnt 2>/dev/null || true; \
                              umount -l /tmp/efi_mnt 2>/dev/null || true; \
                              mount -t btrfs -o subvol=@ \"$ROOT_PART\" /tmp/root_mnt; \
                              mount \"$EFI_PART\" /tmp/efi_mnt; \
                              DEPLOY_PATH=$(find /tmp/root_mnt/ostree/deploy/default/deploy -maxdepth 1 -name '*.0' -type d | head -n1); \
                              [ -z \"$DEPLOY_PATH\" ] && echo 'Error: Deploy path not found' && exit 1; \
                              mkdir -p \"$DEPLOY_PATH/sysroot\" \"$DEPLOY_PATH/ostree\"; \
                              sed -i 's/transient=true/transient=false/g' /tmp/root_mnt/ostree/repo/config 2>/dev/null || true; \
                              if [ \"{grub}\" = \"true\" ] && ! echo \"$ROOT_PART\" | grep -q '^/dev/mapper/'; then \
                                sed -i 's/bootloader=none/bootloader=grub2/' /tmp/root_mnt/ostree/repo/config; \
                                grub-install --target=x86_64-efi --efi-directory=/tmp/efi_mnt --bootloader-id=ARCHLINUX --boot-directory=/tmp/root_mnt/boot --recheck; \
                                ROOT_UUID=$(blkid -s UUID -o value \"$ROOT_PART\") && \
                                VMLINUZ=$(find /tmp/root_mnt/boot/ostree -maxdepth 2 -name 'vmlinuz-*' -type f 2>/dev/null | head -n1); \
                                INITRAMFS=$(find /tmp/root_mnt/boot/ostree -maxdepth 2 -name 'initramfs-*' -type f 2>/dev/null | head -n1); \
                                if [ -z \"$VMLINUZ\" ]; then \
                                  echo 'Error: Kernel not found in /boot/ostree' && exit 1; \
                                fi; \
                                OSTREE_PARAM=$(grep -o 'ostree=[^ ]*' /tmp/root_mnt/boot/loader/entries/ostree-*.conf 2>/dev/null | head -n1) || true; \
                                [ -z \"$OSTREE_PARAM\" ] && OSTREE_PARAM=\"ostree=0\"; \
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
                              else \
                                if echo \"$ROOT_PART\" | grep -q '^/dev/mapper/'; then \
                                  echo \"Using systemd-boot for encrypted root.\"; \
                                fi; \
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
                                  if [ -b /dev/mapper/ark-root ]; then \
                                    LUKS_BACKING=$(cryptsetup status ark-root 2>/dev/null | awk '/device:/ {{print $2}}'); \
                                    if [ -n \"$LUKS_BACKING\" ]; then \
                                      LUKS_UUID=$(blkid -s UUID -o value \"$LUKS_BACKING\"); \
                                      for e in /tmp/efi_mnt/loader/entries/*.conf; do \
                                        [ -f \"$e\" ] && grep -q '^options ' \"$e\" && \
                                          sed -i \"/^options /s|$| rd.luks.name=$LUKS_UUID=ark-root|\" \"$e\"; \
                                      done; \
                                    fi; \
                                  fi; \
                                  for e in /tmp/efi_mnt/loader/entries/*.conf; do [ -f \"$e\" ] && grep -q 'title.*ostree:' \"$e\" 2>/dev/null && rm -f \"$e\"; done; \
                                fi; \
                              fi; \
                              FIRST_DEP=$(ls -1d /tmp/root_mnt/ostree/deploy/default/deploy/*.0 2>/dev/null | head -n1 || true); \
                              if [ -n \"$FIRST_DEP\" ]; then \
                                DEP_ID=$(basename \"$FIRST_DEP\"); \
                                BC=\"${{DEP_ID%.*}}\"; \
                                BS=\"${{DEP_ID##*.}}\"; \
                                mkdir -p /tmp/root_mnt/ostree/boot.0/default/$BC 2>/dev/null || true; \
                                ln -sfn \"../../../deploy/default/deploy/$DEP_ID\" /tmp/root_mnt/ostree/boot.0/default/$BC/$BS 2>/dev/null || true; \
                              fi;",
                             disk = disk,
                             grub = if install_grub { "true" } else { "false" }
                         );
                        let _ = tokio::process::Command::new("pkexec")
                            .args(["bash", "-c", &bootloader_cmd])
                            .output()
                            .await;

                        if !install_grub {
                            let bls_with_env = format!(
                                "export SYSROOT=/tmp/root_mnt; export ESP=/tmp/efi_mnt; {}",
                                BLS_SYNC_SCRIPT
                            );
                            let _ = tokio::process::Command::new("pkexec")
                                .args(["bash", "-c", &bls_with_env])
                                .output()
                                .await;
                        }

                        let unmount_cmd = "umount -l /tmp/efi_mnt 2>/dev/null || true; \
                                           umount -l /tmp/root_mnt/boot 2>/dev/null || true; \
                                           umount -l /tmp/root_mnt 2>/dev/null || true";
                        let _ = tokio::process::Command::new("pkexec")
                            .args(["bash", "-c", unmount_cmd])
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

const MIN_DISK_BYTES: u64 = 20 * 1024 * 1024 * 1024;

fn format_bytes(bytes: u64) -> String {
    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;
    if bytes >= GIB {
        format!("{:.1} GB", bytes as f64 / GIB as f64)
    } else {
        format!("{:.0} MB", bytes as f64 / MIB as f64)
    }
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
        if lower.contains("network is unreachable") {
            return Some((None, "Installation Error: Network unreachable. Please check your internet connection and try again.".to_string()));
        }
        if lower.contains("unexpected end of file") {
            return Some((None, "Installation Error: Failed to pull OS image from registry (unexpected EOF). This is usually a transient registry error — please try again.".to_string()));
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
