#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdio.h>
#include <wchar.h>

static int mark(const wchar_t *path, const char *phase) {
    FILE *stream = _wfopen(path, L"ab");
    if (!stream) return 0;
    fprintf(stream, "%s\n", phase);
    fclose(stream);
    return 1;
}

static int has_text(HANDLE screen, const wchar_t *text) {
    wchar_t buffer[32769] = {0};
    DWORD read = 0;
    COORD top = {0,0};
    return ReadConsoleOutputCharacterW(screen, buffer, 32768, top, &read)
        && wcsstr(buffer, text) != NULL;
}

static int wait_text(HANDLE screen, HANDLE child, const wchar_t *text) {
    for (int attempt=0; attempt<600; attempt++) {
        if (has_text(screen, text)) return 1;
        if (WaitForSingleObject(child, 0) != WAIT_TIMEOUT) return 0;
        Sleep(20);
    }
    return 0;
}

static int press(HANDLE input, WORD key, WCHAR character) {
    INPUT_RECORD record = {0};
    record.EventType = KEY_EVENT;
    record.Event.KeyEvent.bKeyDown = TRUE;
    record.Event.KeyEvent.wRepeatCount = 1;
    record.Event.KeyEvent.wVirtualKeyCode = key;
    record.Event.KeyEvent.uChar.UnicodeChar = character;
    DWORD written = 0;
    return WriteConsoleInputW(input, &record, 1, &written) && written == 1;
}

static HWND wait_tray(HANDLE child) {
    for (int attempt=0; attempt<400; attempt++) {
        HWND tray = FindWindowW(L"9RouterTrayWindow", NULL);
        if (tray) return tray;
        if (WaitForSingleObject(child, 0) != WAIT_TIMEOUT) return NULL;
        Sleep(25);
    }
    return NULL;
}

static int attached(DWORD pid) {
    DWORD list[16];
    DWORD count = GetConsoleProcessList(list, 16);
    if (count > 16) return 1;
    for (DWORD i=0; i<count; i++) {
        if (list[i] == pid) return 1;
    }
    return 0;
}

int wmain(int argc, wchar_t **argv) {
    if (argc != 6) return 2;
    if (!AllocConsole() && GetLastError() != ERROR_ACCESS_DENIED) {
        mark(argv[4], "allocate-console-failed");
        return 3;
    }
    SECURITY_ATTRIBUTES inheritable = {sizeof(inheritable),NULL,TRUE};
    HANDLE input = CreateFileW(L"CONIN$",GENERIC_READ | GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,&inheritable,OPEN_EXISTING,0,NULL);
    HANDLE output = CreateFileW(L"CONOUT$",GENERIC_READ | GENERIC_WRITE,
        FILE_SHARE_READ | FILE_SHARE_WRITE,&inheritable,OPEN_EXISTING,0,NULL);
    DWORD original_input, original_output;
    if (input == INVALID_HANDLE_VALUE || output == INVALID_HANDLE_VALUE ||
        !GetConsoleMode(input,&original_input) || !GetConsoleMode(output,&original_output)) {
        mark(argv[4], "console-handles-unavailable");
        return 4;
    }
    original_input &= ~ENABLE_ECHO_INPUT;
    if (!SetConsoleMode(input, original_input)) return 5;
    WCHAR command[4096];
    const wchar_t *tray = wcscmp(argv[3],L"tray") == 0 ? L" --tray" : L"";
    if (swprintf(command,4096,L"\"%ls\" --port %ls --no-browser%ls",argv[1],argv[2],tray) < 0) return 6;
    STARTUPINFOW startup = {0};
    startup.cb = sizeof(startup);
    startup.dwFlags = STARTF_USESTDHANDLES;
    startup.hStdInput = input;
    startup.hStdOutput = output;
    startup.hStdError = output;
    PROCESS_INFORMATION process = {0};
    if (!CreateProcessW(argv[1],command,NULL,NULL,TRUE,0,NULL,NULL,&startup,&process)) {
        mark(argv[4], "spawn-failed");
        return 7;
    }
    CloseHandle(process.hThread);
    int result = 8;
    if (wcscmp(argv[3],L"tray") != 0) {
        if (!wait_text(output,process.hProcess,L"Choose Interface") ||
            !has_text(output,L"Web UI (Open in Browser)") ||
            !has_text(output,L"Terminal UI (Interactive CLI)") ||
            !has_text(output,L"Hide to Tray (Background)") ||
            !has_text(output,L"Exit")) {
            mark(argv[4],"menu-unavailable");
            goto cleanup;
        }
        mark(argv[4],"menu");
        if (wcscmp(argv[3],L"exit") == 0) {
            if (!press(input,VK_DOWN,0) || !press(input,VK_RETURN,L'\r') ||
                !wait_text(output,process.hProcess,L"9Router Terminal UI") ||
                !press(input,VK_ESCAPE,27) || !wait_text(output,process.hProcess,L"Choose Interface") ||
                !press(input,VK_END,0) || !press(input,VK_RETURN,L'\r')) {
                mark(argv[4],"terminal-navigation-failed");
                goto cleanup;
            }
            mark(argv[4],"terminal-and-exit");
        } else {
            if (!press(input,VK_DOWN,0) || !press(input,VK_DOWN,0) || !press(input,VK_RETURN,L'\r')) goto cleanup;
            HWND window = wait_tray(process.hProcess);
            if (!window) {
                mark(argv[4],"tray-unavailable");
                goto cleanup;
            }
            for (int i=0; i<300 && attached(process.dwProcessId); i++) Sleep(20);
            if (attached(process.dwProcessId)) {
                mark(argv[4],"still-attached");
                goto cleanup;
            }
            mark(argv[4],"same-process-detached");
            for (int i=0; i<400 && GetFileAttributesW(argv[5])==INVALID_FILE_ATTRIBUTES; i++) Sleep(20);
            if (GetFileAttributesW(argv[5])==INVALID_FILE_ATTRIBUTES || !PostMessageW(window,WM_COMMAND,102,0)) goto cleanup;
        }
    } else {
        HWND window = wait_tray(process.hProcess);
        if (!window || has_text(output,L"Choose Interface") || !PostMessageW(window,WM_COMMAND,102,0)) goto cleanup;
        mark(argv[4],"explicit-tray");
    }
    DWORD code = 1, input_mode = 0, output_mode = 0;
    if (WaitForSingleObject(process.hProcess,12000)==WAIT_OBJECT_0 &&
        GetExitCodeProcess(process.hProcess,&code) && code==0 &&
        GetConsoleMode(input,&input_mode) && input_mode==original_input &&
        GetConsoleMode(output,&output_mode) && output_mode==original_output) {
        mark(argv[4],"restored-and-stopped");
        result = 0;
    }
cleanup:
    if (WaitForSingleObject(process.hProcess,0)==WAIT_TIMEOUT) {
        TerminateProcess(process.hProcess,100);
        WaitForSingleObject(process.hProcess,5000);
    }
    CloseHandle(process.hProcess);
    CloseHandle(input);
    CloseHandle(output);
    FreeConsole();
    return result;
}
