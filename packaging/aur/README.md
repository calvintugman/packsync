# AUR Package (`packsync-bin`)

Every GitHub Release automatically builds a ready-to-publish `PKGBUILD` and `.SRCINFO` with the exact `sha256sums` for `packsync-<version>-linux-x86_64.tar.gz`.

## Publishing to the AUR

1. Create an account on [aur.archlinux.org](https://aur.archlinux.org/) and add your SSH public key in your AUR account settings.
2. Clone the AUR package repository (this creates the package on first push):
   ```sh
   git clone ssh://aur@aur.archlinux.org/packsync-bin.git
   cd packsync-bin
   ```
3. Copy `PKGBUILD` and `.SRCINFO` (from `packaging/aur/` or from the GitHub Release assets) into that directory, commit, and push:
   ```sh
   git add PKGBUILD .SRCINFO
   git commit -m "Release packsync-bin 0.2.1"
   git push -u origin master
   ```

Users on Arch Linux can then install with:
```sh
yay -S packsync-bin
# or
paru -S packsync-bin
```
