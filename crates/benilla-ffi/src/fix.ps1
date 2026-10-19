$src = "I:\AIÓÎÏ·\benilla\crates\benilla-ffi\src\lib.rs"
$c = Get-Content $src -Raw

# 1. Replace wrong wow_to_ue + add wow_vec_to_ue
$old1 = "/// Convert a WoW Y-up position to UE Z-up: (x, z, -y).`nfn wow_to_ue(px: f32, py: f32, pz: f32) -> BnVertex {`n    BnVertex {`n        x: px,`n        y: pz,`n        z: -py,`n        nx: 0.0,`n        ny: 1.0,`n        nz: 0.0,`n        u: 0.0,`n        v: 0.0,`n    }`n}"

$new1 = "/// Convert a WoW position to UE Y-up: `ue = (-wow_y, wow_z, -wow_x)`.`n/// WoW axes: +X north, +Y west, +Z up. UE axes: +X right, +Y up, +Z forward.`nfn wow_to_ue(px: f32, py: f32, pz: f32) -> BnVertex {`n    BnVertex {`n        x: -py,`n        y: pz,`n        z: -px,`n        nx: 0.0,`n        ny: 1.0,`n        nz: 0.0,`n        u: 0.0,`n        v: 0.0,`n    }`n}`n`n/// Convert a WoW normal/vector to UE Y-up (same axis mapping as `wow_to_ue`).`nfn wow_vec_to_ue(vx: f32, vy: f32, vz: f32) -> (f32, f32, f32) {`n    (-vy, vz, -vx)`n}"

if ($c.Contains($old1)) { $c = $c.Replace($old1, $new1); Write-Host "1: wow_to_ue fixed" }
else { Write-Host "1: NOT FOUND - checking for CRLF" }

# 2. Fix WMO vertex fill
$old2 = @"
                    unsafe {
                        *out_vertices.add(i) = BnVertex {
                            x: v.x,
                            y: v.z,
                            z: -v.y,
                            nx: vn.0,
                            ny: vn.2,
                            nz: -vn.1,
                            u: uv.0,
                            v: uv.1,
                        };
                    }
"@
$new2 = @"
                    let (wx, wy, wz) = wow_vec_to_ue(v.x, v.y, v.z);
                    let (wnx, wny, wnz) = wow_vec_to_ue(vn.0, vn.1, vn.2);
                    // SAFETY: out_vertices has room for `cap` BnVertex.
                    unsafe {
                        *out_vertices.add(i) = BnVertex {
                            x: wx,
                            y: wy,
                            z: wz,
                            nx: wnx,
                            ny: wny,
                            nz: wnz,
                            u: uv.0,
                            v: uv.1,
                        };
                    }
"@
if ($c.Contains($old2)) { $c = $c.Replace($old2, $new2); Write-Host "2: WMO fill fixed" }
else { Write-Host "2: WMO NOT FOUND" }

# 3. Fix bn_model_fill vertex output
$old3 = @"
                    // Convert WoW Y-up to UE Z-up: (x, z, -y).
                    // SAFETY: out_vertices has room for `cap` BnVertex.
                    unsafe {
                        *out_vertices.add(i) = BnVertex {
                            x: wx,
                            y: wz,
                            z: -wy,
                            nx: v.normal.x,
                            ny: v.normal.z,
                            nz: -v.normal.y,
                            u: v.tex_coords.x,
                            v: v.tex_coords.y,
                        };
                    }
"@
$new3 = @"
                    // Convert WoW to UE Y-up: (-y, z, -x).
                    let (q_x, q_y, q_z) = wow_vec_to_ue(wx, wy, wz);
                    let (n_x, n_y, n_z) = wow_vec_to_ue(
                        v.normal.x, v.normal.y, v.normal.z,
                    );
                    // SAFETY: out_vertices has room for `cap` BnVertex.
                    unsafe {
                        *out_vertices.add(i) = BnVertex {
                            x: q_x,
                            y: q_y,
                            z: q_z,
                            nx: n_x,
                            ny: n_y,
                            nz: n_z,
                            u: v.tex_coords.x,
                            v: v.tex_coords.y,
                        };
                    }
"@
if ($c.Contains($old3)) { $c = $c.Replace($old3, $new3); Write-Host "3: bn_model_fill fixed" }
else { Write-Host "3: model fill NOT FOUND" }

Set-Content $src $c -Encoding UTF8 -NoNewline
Write-Host "Saved."

# Verify
Select-String -Path $src -Pattern "wow_vec_to_ue|q_x|q_y|q_z|n_x|n_y|n_z" | Select-Object LineNumber, Line
