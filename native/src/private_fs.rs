use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::Path,
};

pub(crate) fn reject_link(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() {
        return Err(io::Error::other("Protected path cannot be a link"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(io::Error::other("Protected path cannot be a reparse point"));
        }
    }
    Ok(())
}

pub(crate) fn directory(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    reject_link(path)?;
    restrict(path, true)
}

#[cfg(unix)]
pub(crate) fn restrict(path: &Path, dir: bool) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if dir { 0o700 } else { 0o600 }),
    )
}

#[cfg(windows)]
pub(crate) fn restrict(path: &Path, dir: bool) -> io::Result<()> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, HANDLE, LocalFree},
        Security::Authorization::{
            ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
            SDDL_REVISION_1, SE_FILE_OBJECT, SetNamedSecurityInfoW,
        },
        Security::{
            DACL_SECURITY_INFORMATION, GetSecurityDescriptorDacl, GetTokenInformation,
            PROTECTED_DACL_SECURITY_INFORMATION, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };
    struct TokenHandle(HANDLE);
    impl Drop for TokenHandle {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    struct Local(*mut c_void);
    impl Drop for Local {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    // SAFETY: Win32 buffers live throughout each call; TOKEN_USER uses an aligned
    // allocation, and every successful allocation/handle has exactly one owner.
    unsafe {
        let mut token = ptr::null_mut();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
            return Err(io::Error::last_os_error());
        }
        let token = TokenHandle(token);
        let mut needed = 0;
        GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut needed);
        if needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let mut data = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
        if GetTokenInformation(
            token.0,
            TokenUser,
            data.as_mut_ptr().cast(),
            needed,
            &mut needed,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = &*data.as_ptr().cast::<TOKEN_USER>();
        let mut sid = ptr::null_mut();
        if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
            return Err(io::Error::last_os_error());
        }
        let sid_alloc = Local(sid.cast());
        let mut len = 0;
        while *sid.add(len) != 0 {
            len += 1;
        }
        let sid = String::from_utf16_lossy(std::slice::from_raw_parts(sid, len));
        drop(sid_alloc);
        // A protected DACL containing only the service account, inherited by
        // files/subdirectories. No inherited Users/Everyone/Administrators ACE.
        let inheritance = if dir { "OICI" } else { "" };
        let sddl: Vec<u16> = format!("D:P(A;{inheritance};FA;;;{sid})")
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut descriptor = ptr::null_mut();
        if ConvertStringSecurityDescriptorToSecurityDescriptorW(
            sddl.as_ptr(),
            SDDL_REVISION_1,
            &mut descriptor,
            ptr::null_mut(),
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }
        let _descriptor = Local(descriptor);
        let mut present = 0;
        let mut defaulted = 0;
        let mut acl = ptr::null_mut();
        if GetSecurityDescriptorDacl(descriptor, &mut present, &mut acl, &mut defaulted) == 0
            || present == 0
            || acl.is_null()
        {
            return Err(io::Error::other("Private DACL unavailable"));
        }
        let name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let result = SetNamedSecurityInfoW(
            name.as_ptr(),
            SE_FILE_OBJECT,
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            ptr::null_mut(),
            ptr::null_mut(),
            acl,
            ptr::null(),
        );
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result as i32));
        }
    }
    Ok(())
}

pub(crate) fn create_new(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    if let Err(error) = restrict(path, false) {
        let _ = fs::remove_file(path);
        return Err(error);
    }
    Ok(file)
}

/// Atomic replacement inside an already protected directory. A failure leaves
/// either the old complete file or the new complete file, never partial secrets.
pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("Missing parent"))?;
    let temporary = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut file = create_new(&temporary)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use std::{os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::CloseHandle,
        Foundation::LocalFree,
        Security::Authorization::{GetNamedSecurityInfoW, SE_FILE_OBJECT},
        Security::{
            ACCESS_ALLOWED_ACE, DACL_SECURITY_INFORMATION, EqualSid, GetAce,
            GetSecurityDescriptorControl, GetTokenInformation, SE_DACL_PROTECTED, TOKEN_QUERY,
            TOKEN_USER, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    #[test]
    fn windows_acl_is_protected_and_grants_only_the_current_service_user() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("private");
        directory(&folder).unwrap();
        let path = folder.join("secret");
        create_new(&path).unwrap().write_all(b"fixture").unwrap();
        // Read the ACL back from Windows and compare its trustee with the
        // current process token, independently of the SDDL creation path.
        unsafe {
            let mut token = ptr::null_mut();
            assert_ne!(
                OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token),
                0
            );
            let mut needed = 0;
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut needed);
            let mut data = vec![0usize; (needed as usize).div_ceil(size_of::<usize>())];
            assert_ne!(
                GetTokenInformation(
                    token,
                    TokenUser,
                    data.as_mut_ptr().cast(),
                    needed,
                    &mut needed
                ),
                0
            );
            let user = &*data.as_ptr().cast::<TOKEN_USER>();
            for (file, inheritance) in [(&folder, 3u8), (&path, 0u8)] {
                let name: Vec<u16> = file.as_os_str().encode_wide().chain(Some(0)).collect();
                let mut acl = ptr::null_mut();
                let mut descriptor = ptr::null_mut();
                assert_eq!(
                    GetNamedSecurityInfoW(
                        name.as_ptr(),
                        SE_FILE_OBJECT,
                        DACL_SECURITY_INFORMATION,
                        ptr::null_mut(),
                        ptr::null_mut(),
                        &mut acl,
                        ptr::null_mut(),
                        &mut descriptor
                    ),
                    0
                );
                assert!(!acl.is_null());
                let mut control = 0;
                let mut revision = 0;
                assert_ne!(
                    GetSecurityDescriptorControl(descriptor, &mut control, &mut revision),
                    0
                );
                assert_ne!(control & SE_DACL_PROTECTED, 0);
                assert_eq!((*acl).AceCount, 1);
                let mut ace = ptr::null_mut();
                assert_ne!(GetAce(acl, 0, &mut ace), 0);
                let allowed = &*ace.cast::<ACCESS_ALLOWED_ACE>();
                assert_eq!(allowed.Header.AceType, 0); // ACCESS_ALLOWED_ACE_TYPE
                assert_eq!(allowed.Header.AceFlags, inheritance);
                assert_eq!(allowed.Mask, 0x1f01ff); // FILE_ALL_ACCESS
                assert_ne!(
                    EqualSid(
                        (&raw const allowed.SidStart).cast_mut().cast(),
                        user.User.Sid
                    ),
                    0
                );
                LocalFree(descriptor);
            }
            CloseHandle(token);
        }
    }
}
