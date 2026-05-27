# Publishing HyperPod

This document covers the two remaining manual steps: publishing to **crates.io**
and submitting to **WinGet**.

---

## 1. crates.io

### One-time setup

```powershell
# Log in with your crates.io API token (from https://crates.io/me)
cargo login <your-token>
```

### Dry-run (always run first)

```powershell
cd hyperpod
cargo publish --dry-run
```

Common reasons a dry-run fails:
* `readme` path is relative to the workspace root (`../README.md`) — crates.io
  requires the file to exist relative to the crate root *or* be an absolute URL.
  If the dry-run errors on this, either copy `README.md` into `hyperpod/` or
  change `readme` to `"README.md"` and place the file there.
* The crate name `hyperpod` must not already be claimed on crates.io.

### Publish

```powershell
cd hyperpod
cargo publish
```

The crate will be live at `https://crates.io/crates/hyperpod` within ~30 seconds.

---

## 2. WinGet (microsoft/winget-pkgs)

### Step 1 — Build the release binary

```powershell
cd hyperpod
cargo build --release --target x86_64-pc-windows-msvc
# Binary: target\x86_64-pc-windows-msvc\release\hyperpod.exe
```

### Step 2 — Create a GitHub Release

1. Tag the commit:
   ```powershell
   git tag v0.1.0
   git push origin v0.1.0
   ```
2. On GitHub → Releases → Draft a new release → choose tag `v0.1.0`.
3. Upload `hyperpod.exe` as a release asset.
4. Note the download URL — it will be:
   `https://github.com/turtle170/HyperPod/releases/download/v0.1.0/hyperpod-x86_64-pc-windows-msvc.exe`
   (rename the file to match before uploading).

### Step 3 — Compute SHA-256

```powershell
Get-FileHash .\target\x86_64-pc-windows-msvc\release\hyperpod.exe -Algorithm SHA256 |
    Select-Object -ExpandProperty Hash
```

### Step 4 — Update the installer manifest

Edit `manifests/h/HyperPod/HyperPod/0.1.0/HyperPod.HyperPod.installer.yaml`:
* Replace the placeholder `InstallerSha256` with the real hash (lowercase).
* Verify `InstallerUrl` matches the uploaded asset URL exactly.
* Update `ReleaseDate` to the actual release date (YYYY-MM-DD).

### Step 5 — Validate locally with winget-cli

```powershell
winget validate --manifest manifests\h\HyperPod\HyperPod\0.1.0\
```

Fix any validation errors before submitting.

### Step 6 — Submit to microsoft/winget-pkgs

```powershell
# Fork https://github.com/microsoft/winget-pkgs on GitHub, then:
git clone https://github.com/<your-fork>/winget-pkgs
cd winget-pkgs

# Copy the three manifest files into the right place
$dest = "manifests\h\HyperPod\HyperPod\0.1.0"
New-Item -ItemType Directory -Force $dest
Copy-Item C:\Users\Account_2\HyperPod\manifests\h\HyperPod\HyperPod\0.1.0\* $dest

git checkout -b add-HyperPod-0.1.0
git add $dest
git commit -m "Add HyperPod.HyperPod version 0.1.0"
git push origin add-HyperPod-0.1.0
```

Then open a Pull Request from your fork to `microsoft/winget-pkgs`.
The bot will validate the manifest and a human reviewer will merge it.

---

## 3. Checklist

- [ ] `cargo login` done
- [ ] `cargo publish --dry-run` passes
- [ ] `cargo publish` done
- [ ] GitHub Release `v0.1.0` created with binary asset
- [ ] `InstallerSha256` updated in installer manifest
- [ ] `winget validate` passes
- [ ] PR opened to `microsoft/winget-pkgs`
