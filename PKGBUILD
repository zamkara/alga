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
  cargo build --release --locked
}

package() {
  cd "$pkgname"
  install -Dm755 "target/release/alga" "$pkgdir/usr/bin/alga"
  install -Dm644 "data/alga.svg" "$pkgdir/usr/share/icons/hicolor/scalable/apps/alga.svg" || true
  install -Dm644 "data/alga.desktop" "$pkgdir/usr/share/applications/alga.desktop" || true
}
