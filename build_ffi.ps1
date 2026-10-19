$env:RUSTUP_HOME = "D:\Rust\rustup"
$env:CARGO_HOME   = "D:\Rust\cargo"
$env:PATH = "D:\Rust\cargo\bin;" + $env:PATH
Set-Location "I:\AIÓÎÏ·\benilla\crates\benilla-ffi"
cargo build --release 2>&1
Write-Host "Exit: $LASTEXITCODE"
