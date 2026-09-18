use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use std::{env, ptr};

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegSetValueExW,
};
use windows_sys::Win32::UI::Shell::{
    SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify, ShellExecuteW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

const PROG_ID: &str = "MarkdownEditor.Markdown";
const REGISTERED_APP_NAME: &str = "Markdown Editor";

pub fn register_and_open_default_apps() -> Result<(), String> {
    let executable = env::current_exe().map_err(|error| error.to_string())?;
    register(&executable)?;

    // Inform Explorer before opening Settings so the newly registered app is visible immediately.
    // SAFETY: SHCNF_IDLIST with two null pointers means "no specific items";
    // the event and flags are documented constants and the call touches no
    // memory of ours.
    unsafe {
        SHChangeNotify(
            SHCNE_ASSOCCHANGED as i32,
            SHCNF_IDLIST,
            ptr::null::<c_void>(),
            ptr::null::<c_void>(),
        );
    }

    open_default_apps_settings()
}

/// 打开“默认应用”设置页。
///
/// 不能交给 explorer.exe：Explorer 把自己的命令行参数按文件系统路径解析，
/// `ms-settings:…` 解析不出路径时会退化成打开一个文件夹窗口（“文档”），
/// 而不是设置页。ShellExecuteW 走 Shell 的协议处理器，才会唤起“设置”。
fn open_default_apps_settings() -> Result<(), String> {
    let operation = wide("open");
    let target = wide(&default_apps_settings_uri());
    // SAFETY: 两个宽字符缓冲区都以 NUL 结尾，且在本函数返回前一直有效；
    // 父窗口与工作目录传空指针是 API 允许的取值。
    let result = unsafe {
        ShellExecuteW(
            ptr::null_mut(),
            operation.as_ptr(),
            target.as_ptr(),
            ptr::null(),
            ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 返回值小于等于 32 表示失败，见官方文档的返回值表。
    if result as isize <= 32 {
        return Err(format!(
            "打开默认应用设置失败（ShellExecuteW 返回 {}）",
            result as isize
        ));
    }
    Ok(())
}

fn default_apps_settings_uri() -> String {
    // 查询参数中的空格必须转义，否则“设置”定位不到已注册的应用。
    let registered_app = REGISTERED_APP_NAME.replace(' ', "%20");
    format!("ms-settings:defaultapps?registeredAppUser={registered_app}")
}

fn register(executable: &Path) -> Result<(), String> {
    let executable = executable
        .to_str()
        .ok_or_else(|| "应用路径不是有效的 Unicode".to_string())?;
    let command = open_command(executable);
    let icon = format!("{executable},0");

    // ProgID 键路径统一由常量拼接，避免字符串字面量与 PROG_ID 漂移。
    let classes_key = format!("Software\\Classes\\{PROG_ID}");
    let classes_icon_key = format!("Software\\Classes\\{PROG_ID}\\DefaultIcon");
    let classes_command_key = format!("Software\\Classes\\{PROG_ID}\\shell\\open\\command");
    let string_values = [
        (classes_key.as_str(), "", "Markdown 文档"),
        (classes_icon_key.as_str(), "", icon.as_str()),
        (classes_command_key.as_str(), "", command.as_str()),
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
    // SAFETY: every pointer is either null (an accepted "no value" argument)
    // or a null-terminated wide buffer owned by the locals below and valid for
    // the call; `&mut key` receives a handle we then close.
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
    // SAFETY: `key` is a live handle from RegCreateKeyExW above; the value
    // pointer covers exactly `bytes` wide characters of the null-terminated
    // buffer held by `value`.
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
    // SAFETY: `key` is the handle opened above and is closed exactly once on
    // every path through this function.
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
    fn 默认应用设置地址转义注册名中的空格() {
        assert_eq!(
            default_apps_settings_uri(),
            "ms-settings:defaultapps?registeredAppUser=Markdown%20Editor"
        );
    }

    #[test]
    fn 注册命令正确引用应用和文件路径() {
        let executable = r"C:\Program Files\Markdown Editor\markdown-editor.exe";
        assert_eq!(
            open_command(executable),
            r#""C:\Program Files\Markdown Editor\markdown-editor.exe" "%1""#
        );
    }
}
