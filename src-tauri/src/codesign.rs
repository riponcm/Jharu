//! The developer team that signed a macOS program, read from its code
//! signature.
//!
//! Group containers are named after the signing team ("UBF8T346G9.Office"),
//! not after an app, so telling whether their app is still installed needs
//! each installed app's team. `codesign -dv` would cost a process per app;
//! the team is a short string in the signature's CodeDirectory, found by
//! reading a few small pieces of the program file. Nothing is executed and
//! the signature is not verified: this only names the team.

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const FAT_MAGIC: u32 = 0xCAFE_BABE;
const FAT_MAGIC_64: u32 = 0xCAFE_BABF;
const MH_MAGIC_64: u32 = 0xFEED_FACF;
const MH_MAGIC: u32 = 0xFEED_FACE;
const LC_CODE_SIGNATURE: u32 = 0x1D;
const CSMAGIC_EMBEDDED_SIGNATURE: u32 = 0xFADE_0CC0;
const CSMAGIC_CODEDIRECTORY: u32 = 0xFADE_0C02;
const CSSLOT_CODEDIRECTORY: u32 = 0;
/// CodeDirectory versions from this one on carry a team offset.
const CS_SUPPORTSTEAMID: u32 = 0x20200;

/// The team ID of an app bundle's main program, if it is signed by a team.
/// Apple's own programs and ad-hoc signed ones have none.
pub fn team_of_app(app: &Path, executable: Option<&str>) -> Option<String> {
    let exe = app.join("Contents").join("MacOS").join(executable?);
    let mut f = std::fs::File::open(exe).ok()?;
    team_of(&mut f)
}

fn read_at<R: Read + Seek>(r: &mut R, at: u64, len: usize) -> Option<Vec<u8>> {
    r.seek(SeekFrom::Start(at)).ok()?;
    let mut buf = vec![0u8; len];
    r.read_exact(&mut buf).ok()?;
    Some(buf)
}

fn be(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn le(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

/// Reads the team from a Mach-O program, thin or universal.
pub fn team_of<R: Read + Seek>(r: &mut R) -> Option<String> {
    let head = read_at(r, 0, 8)?;
    let slice = match be(&head, 0)? {
        // Universal: every slice is signed by the same team; take the first.
        FAT_MAGIC => be(&read_at(r, 8, 20)?, 8)? as u64,
        FAT_MAGIC_64 => {
            let a = read_at(r, 8, 32)?;
            ((be(&a, 8)? as u64) << 32) | be(&a, 12)? as u64
        }
        _ => 0,
    };
    let mh = read_at(r, slice, 32)?;
    let header_len = match le(&mh, 0)? {
        MH_MAGIC_64 => 32,
        MH_MAGIC => 28,
        _ => return None,
    };
    let (ncmds, sizeofcmds) = (le(&mh, 16)? as usize, le(&mh, 20)? as usize);
    if sizeofcmds > 1 << 20 {
        return None;
    }
    let cmds = read_at(r, slice + header_len, sizeofcmds)?;
    let mut at = 0usize;
    let mut sig = None;
    for _ in 0..ncmds {
        let (cmd, size) = (le(&cmds, at)?, le(&cmds, at + 4)? as usize);
        if cmd == LC_CODE_SIGNATURE {
            sig = Some((le(&cmds, at + 8)? as u64, le(&cmds, at + 12)? as usize));
            break;
        }
        if size < 8 {
            return None;
        }
        at += size;
    }
    let (dataoff, datasize) = sig?;
    let sig_at = slice + dataoff;

    // The embedded signature: a blob index pointing at the CodeDirectory.
    let sb = read_at(r, sig_at, 12)?;
    if be(&sb, 0)? != CSMAGIC_EMBEDDED_SIGNATURE {
        return None;
    }
    let count = (be(&sb, 8)? as usize).min(64);
    let index = read_at(r, sig_at + 12, count * 8)?;
    let cd_off = (0..count).find(|i| be(&index, i * 8) == Some(CSSLOT_CODEDIRECTORY)).and_then(|i| be(&index, i * 8 + 4))? as u64;
    if cd_off as usize >= datasize {
        return None;
    }
    let cd = read_at(r, sig_at + cd_off, 52)?;
    if be(&cd, 0)? != CSMAGIC_CODEDIRECTORY || be(&cd, 8)? < CS_SUPPORTSTEAMID {
        return None;
    }
    let team_off = be(&cd, 48)? as u64;
    if team_off == 0 {
        return None;
    }
    let raw = read_at(r, sig_at + cd_off + team_off, 64).or_else(|| {
        // The name may sit near the end of the file; read what is there.
        let mut buf = vec![];
        r.seek(SeekFrom::Start(sig_at + cd_off + team_off)).ok()?;
        r.take(64).read_to_end(&mut buf).ok()?;
        Some(buf)
    })?;
    let end = raw.iter().position(|&b| b == 0)?;
    let team = std::str::from_utf8(&raw[..end]).ok()?;
    (!team.is_empty() && team.chars().all(|c| c.is_ascii_alphanumeric())).then(|| team.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    /// A minimal signed 64-bit Mach-O: header, one LC_CODE_SIGNATURE, and a
    /// signature holding one CodeDirectory with a team.
    fn macho(team: Option<&str>, version: u32) -> Vec<u8> {
        let mut f = vec![];
        let sig_at = 0x100u32;
        f.extend(MH_MAGIC_64.to_le_bytes());
        f.extend([0u8; 12]); // cputype, cpusubtype, filetype
        f.extend(1u32.to_le_bytes()); // ncmds
        f.extend(16u32.to_le_bytes()); // sizeofcmds
        f.extend([0u8; 8]); // flags, reserved
        f.extend(LC_CODE_SIGNATURE.to_le_bytes());
        f.extend(16u32.to_le_bytes());
        f.extend(sig_at.to_le_bytes());
        f.extend(0x200u32.to_le_bytes());
        f.resize(sig_at as usize, 0);
        // SuperBlob with one index entry.
        let cd_off = 20u32;
        f.extend(CSMAGIC_EMBEDDED_SIGNATURE.to_be_bytes());
        f.extend(0x200u32.to_be_bytes());
        f.extend(1u32.to_be_bytes());
        f.extend(CSSLOT_CODEDIRECTORY.to_be_bytes());
        f.extend(cd_off.to_be_bytes());
        // CodeDirectory: 52 bytes of header, then the team string.
        let mut cd = vec![0u8; 52];
        cd[0..4].copy_from_slice(&CSMAGIC_CODEDIRECTORY.to_be_bytes());
        cd[8..12].copy_from_slice(&version.to_be_bytes());
        if team.is_some() {
            cd[48..52].copy_from_slice(&52u32.to_be_bytes());
        }
        f.extend(cd);
        if let Some(t) = team {
            f.extend(t.as_bytes());
            f.push(0);
        }
        f.resize(sig_at as usize + 0x200, 0);
        f
    }

    #[test]
    fn reads_the_team_from_a_thin_program() {
        assert_eq!(team_of(&mut Cursor::new(macho(Some("UBF8T346G9"), 0x20400))).as_deref(), Some("UBF8T346G9"));
        assert_eq!(team_of(&mut Cursor::new(macho(None, 0x20400))), None, "no team: Apple or ad-hoc");
        assert_eq!(team_of(&mut Cursor::new(macho(Some("UBF8T346G9"), 0x20100))), None, "too old to carry one");
        assert_eq!(team_of(&mut Cursor::new(b"#!/bin/sh\necho hi\n".to_vec())), None);
    }

    #[test]
    fn reads_the_first_slice_of_a_universal_program() {
        let thin = macho(Some("JQ525L2MZD"), 0x20400);
        let mut f = vec![];
        f.extend(FAT_MAGIC.to_be_bytes());
        f.extend(1u32.to_be_bytes());
        f.extend([0u8; 8]); // cputype, cpusubtype
        f.extend(0x1000u32.to_be_bytes()); // offset
        f.extend((thin.len() as u32).to_be_bytes());
        f.extend(12u32.to_be_bytes());
        f.resize(0x1000, 0);
        f.extend(thin);
        assert_eq!(team_of(&mut Cursor::new(f)).as_deref(), Some("JQ525L2MZD"));
    }

    /// Read-only look at the apps on this Mac.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore]
    fn smoke_teams_of_installed_apps() {
        for e in std::fs::read_dir("/Applications").unwrap().flatten() {
            let p = e.path();
            if p.extension().is_some_and(|x| x == "app") {
                let exe = plist::Value::from_file(p.join("Contents/Info.plist"))
                    .ok()
                    .and_then(|v| v.as_dictionary()?.get("CFBundleExecutable")?.as_string().map(String::from));
                println!("{:<50} {:?}", p.display(), team_of_app(&p, exe.as_deref()));
            }
        }
    }
}
