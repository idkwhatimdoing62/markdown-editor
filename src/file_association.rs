use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::{env, ptr};

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegSetValueExW,
};
use windows_sys::Win32::UI::Shell::{SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify};

const PROG_ID: &str = "MarkdownEditor.Markdown";
const REGISTERED_APP_NAME: &str = "Markdown Editor";
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub fn register_and_open_default_apps() -> Result<(), String> {
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    register(&executable)?;

    // Inform Explorer before opening Settings so the newly registered app is visible immediately.
    // SAFETY: the notification API accepts null data pointers for the
    // SHCNE_ASSOCCHANGED event; no borrowed memory is dereferenced.
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST,
            ptr::null::<c_void>(),
            ptr::null::<c_void>(),
        );
    }

    Command::new("explorer.exe")
        .arg("ms-settings:defaultapps?registeredAppUser=Markdown%20Editor")
        .creation_flags(CREATE_NO_WINDOW)
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn register(executable: &Path) -> Result<(), String> {
    let executable = executable
        .to_str()
        .ok_or_else(|| "应用路径不是有效的 Unicode".to_string())?;
    let command = open_command(executable);
    let icon = format!("{executable},0");

    let string_values = [
        (
            "Software\\Classes\\MarkdownEditor.Markdown",
            "",
            "Markdown 文档",
        ),
        (
            "Software\\Classes\\MarkdownEditor.Markdown\\DefaultIcon",
            "",
            icon.as_str(),
        ),
        (
            "Software\\Classes\\MarkdownEditor.Markdown\\shell\\open\\command",
            "",
            command.as_str(),
        ),
        (
            "Software\\MarkdownEditor\\Capabilities",
            "ApplicationName",
            REGISTERED_APP_NAME,
        ),
        (
            "Software\\MarkdownEditor\\Capabilities",
            "ApplicationDescription",
            "Markdown 编辑器与预览器",
        ),
        (
            "Software\\MarkdownEditor\\Capabilities",
            "ApplicationIcon",
            icon.as_str(),
        ),
        (
            "Software\\MarkdownEditor\\Capabilities\\FileAssociations",
            ".md",
            PROG_ID,
        ),
        (
            "Software\\MarkdownEditor\\Capabilities\\FileAssociations",
            ".markdown",
            PROG_ID,
        ),
        (
            "Software\\RegisteredApplications",
            REGISTERED_APP_NAME,
            "Software\\MarkdownEditor\\Capabilities",
        ),
    ];
    for (key, name, value) in string_values {
        set_registry_string(key, name, value)?;
    }

    set_registry_string("Software\\Classes\\.md\\OpenWithProgids", PROG_ID, "")?;
    set_registry_string("Software\\Classes\\.markdown\\OpenWithProgids", PROG_ID, "")?;
    Ok(())
}

fn set_registry_string(key_path: &str, value_name: &str, value: &str) -> Result<(), String> {
    let key_path = wide(key_path);
    let value_name = wide(value_name);
    let value = wide(value);
    let mut key: HKEY = ptr::null_mut();
    // SAFETY: all pointers refer to NUL-terminated UTF-16 buffers owned by
    // this function, and `key` is a valid out-parameter for the registry API.
    let create_result = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            key_path.as_ptr(),
            0,
            ptr::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            ptr::null(),
            &mut key,
            ptr::null_mut(),
        )
    };
    if create_result != ERROR_SUCCESS {
        return Err(format!("注册表键创建失败（错误 {create_result}）"));
    }

    let bytes = value.len().saturating_mul(size_of::<u16>());
    // SAFETY: `key` was returned by RegCreateKeyExW, and the value buffers are
    // NUL-terminated UTF-16 strings whose byte length is explicitly supplied.
    let set_result = unsafe {
        RegSetValueExW(
            key,
            value_name.as_ptr(),
            0,
            REG_SZ,
            value.as_ptr().cast(),
            bytes as u32,
        )
    };
    // SAFETY: `key` is either a valid handle returned above or null after a
    // failed creation; RegCloseKey is only called after the success check.
    unsafe {
        RegCloseKey(key);
    }
    if set_result != ERROR_SUCCESS {
        return Err(format!("注册表值写入失败（错误 {set_result}）"));
    }
    Ok(())
}

fn wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn open_command(executable: &str) -> String {
    format!("\"{executable}\" \"%1\"")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 注册命令正确引用应用和文件路径() {
        let executable = r"C:\Program Files\Markdown Editor\markdown-editor.exe";
        assert_eq!(
            open_command(executable),
            r#""C:\Program Files\Markdown Editor\markdown-editor.exe" "%1""#
        );
    }
}
