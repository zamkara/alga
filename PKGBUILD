pkgname=alga
pkgver=1.0.0
pkgrel=2
pkgdesc="A GTK4/Libadwaita frontend for bootc operations"
arch=('x86_64')
url="https://github.com/zamkara/alga"
license=('MIT')
depends=('gtk4' 'libadwaita' 'vte3' 'polkit')
makedepends=('cargo' 'git')
source=("git+https://github.com/zamkara/alga.git")
md5sums=('SKIP')

build() {
  cd "$pkgname"
  CFLAGS="" RUSTFLAGS="-C link-arg=-fuse-ld=bfd" cargo build --release --locked
}

package() {
  cd "$pkgname"
  install -Dm755 "target/release/alga" "$pkgdir/usr/bin/alga"
  install -Dm644 "data/alga.svg" "$pkgdir/usr/share/icons/hicolor/scalable/apps/com.zamkara.alga.svg" || true
  install -Dm644 "data/com.zamkara.alga.desktop" "$pkgdir/usr/share/applications/com.zamkara.alga.desktop" || true
  install -Dm644 "data/ready-to-go.svg" "$pkgdir/usr/share/alga/ready-to-go.svg" || true
  install -Dm644 "data/check-for-update.svg" "$pkgdir/usr/share/alga/check-for-update.svg" || true
  install -Dm644 "data/update-available.svg" "$pkgdir/usr/share/alga/update-available.svg" || true
}
