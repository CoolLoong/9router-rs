#define WIN32_LEAN_AND_MEAN
#define COBJMACROS
#include <windows.h>
#include <objbase.h>
#include <shlobj.h>
#include <stdbool.h>
#include <stdio.h>
#include <wchar.h>

static int shortcut(const wchar_t *path, const wchar_t *expected_exe) {
    IShellLinkW *link = NULL;
    IPersistFile *file = NULL;
    HRESULT initialized = CoInitializeEx(NULL, COINIT_APARTMENTTHREADED);
    int result = 6;
    if (FAILED(initialized)) return result;
    if (SUCCEEDED(CoCreateInstance(&CLSID_ShellLink, NULL, CLSCTX_INPROC_SERVER,
                                  &IID_IShellLinkW, (void **)&link)) &&
        SUCCEEDED(IShellLinkW_QueryInterface(link, &IID_IPersistFile, (void **)&file)) &&
        SUCCEEDED(IPersistFile_Load(file, path, STGM_READ))) {
        wchar_t executable[1024] = {0};
        wchar_t item_path[MAX_PATH] = {0};
        wchar_t arguments[128] = {0};
        PIDLIST_ABSOLUTE item = NULL;
        IShellLinkW_GetPath(link, executable, 1024, NULL, SLGP_RAWPATH);
        if (SUCCEEDED(IShellLinkW_GetIDList(link, &item)) && item) {
            SHGetPathFromIDListW(item, item_path);
            CoTaskMemFree(item);
        }
        unsigned char header[76] = {0};
        FILE *stream = _wfopen(path, L"rb");
        bool header_read = stream && fread(header, 1, sizeof(header), stream) == sizeof(header);
        if (stream) fclose(stream);
        unsigned show = header[60] | (unsigned)header[61] << 8 |
                        (unsigned)header[62] << 16 | (unsigned)header[63] << 24;
        if (SUCCEEDED(IShellLinkW_GetArguments(link, arguments, 128)) &&
            (_wcsicmp(executable, expected_exe) == 0 || _wcsicmp(item_path, expected_exe) == 0) &&
            wcscmp(arguments, L"--tray --skip-update --no-browser") == 0 &&
            header_read && show == SW_SHOWMINNOACTIVE) {
            puts("Unicode shortcut target and launch arguments matched");
            result = 0;
        } else {
            char expected[4096] = {0}, target[4096] = {0}, item_target[4096] = {0}, args[512] = {0};
            WideCharToMultiByte(CP_UTF8, 0, expected_exe, -1, expected, sizeof(expected), NULL, NULL);
            WideCharToMultiByte(CP_UTF8, 0, executable, -1, target, sizeof(target), NULL, NULL);
            WideCharToMultiByte(CP_UTF8, 0, item_path, -1, item_target, sizeof(item_target), NULL, NULL);
            WideCharToMultiByte(CP_UTF8, 0, arguments, -1, args, sizeof(args), NULL, NULL);
            fprintf(stderr, "Expected target [%s], read [%s] PIDL [%s] args [%s] show %u\n",
                    expected, target, item_target, args, show);
        }
    }
    if (file) IPersistFile_Release(file);
    if (link) IShellLinkW_Release(link);
    CoUninitialize();
    return result;
}

int wmain(int argc, wchar_t **argv) {
    if (argc == 2 && wcscmp(argv[1], L"startup-path") == 0) {
        PWSTR path = NULL;
        if (FAILED(SHGetKnownFolderPath(&FOLDERID_Startup, 0, NULL, &path))) return 7;
        wprintf(L"%ls\n", path);
        CoTaskMemFree(path);
        return 0;
    }
    if (argc == 4 && wcscmp(argv[1], L"shortcut") == 0) return shortcut(argv[2], argv[3]);
    HWND window = FindWindowW(L"9RouterTrayWindow", NULL);
    if (argc != 2 || window == NULL || IsWindowVisible(window)) {
        return 2;
    }

    if (wcscmp(argv[1], L"status") == 0) {
        DWORD_PTR result = 0;
        if (!SendMessageTimeoutW(window, WM_COMMAND, 100, 0, SMTO_ABORTIFHUNG, 3000, &result)) {
            return 3;
        }
        puts("hidden tray window responded");
        return 0;
    }
    if (wcscmp(argv[1], L"quit") == 0) {
        return PostMessageW(window, WM_COMMAND, 102, 0) ? 0 : 4;
    }
    if (wcscmp(argv[1], L"autostart") == 0) {
        DWORD_PTR result = 0;
        return SendMessageTimeoutW(window, WM_COMMAND, 103, 0, SMTO_ABORTIFHUNG, 3000, &result) ? 0 : 8;
    }
    return 5;
}
