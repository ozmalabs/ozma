#define WIN32_LEAN_AND_MEAN
#include <windows.h>
#include <stdio.h>

static HWND tray_window;

static DWORD WINAPI open_context_menu(void *unused) {
    (void)unused;
    SendMessageW(tray_window, WM_APP + 7, 1, WM_RBUTTONUP);
    return 0;
}

int wmain(int argc, wchar_t **argv) {
    if (argc != 2) return 10;
    if (FindWindowW(L"OzmaDesktopTrayWindow", NULL)) return 18;
    STARTUPINFOW startup = { .cb = sizeof(startup) };
    PROCESS_INFORMATION process = {0};
    wchar_t command[32768];
    if (swprintf(command, 32768, L"\"%ls\"", argv[1]) < 0) return 11;
    if (!CreateProcessW(NULL, command, NULL, NULL, FALSE, CREATE_NO_WINDOW,
                        NULL, NULL, &startup, &process)) {
        fwprintf(stderr, L"CreateProcessW failed: %lu\n", GetLastError());
        return 12;
    }

    for (int i = 0; i < 100 && !tray_window; ++i) {
        tray_window = FindWindowW(L"OzmaDesktopTrayWindow", NULL);
        Sleep(100);
    }
    if (!tray_window) {
        TerminateProcess(process.hProcess, 90);
        return 13;
    }
    DWORD owner_pid = 0;
    GetWindowThreadProcessId(tray_window, &owner_pid);
    if (owner_pid != process.dwProcessId) {
        TerminateProcess(process.hProcess, 94);
        return 19;
    }

    HANDLE trigger = CreateThread(NULL, 0, open_context_menu, NULL, 0, NULL);
    if (!trigger) {
        TerminateProcess(process.hProcess, 91);
        return 14;
    }
    HWND menu = NULL;
    for (int i = 0; i < 50 && !menu; ++i) {
        menu = FindWindowW(L"#32768", NULL);
        Sleep(100);
    }
    if (!menu || !IsWindowVisible(menu)) {
        TerminateProcess(process.hProcess, 92);
        return 15;
    }

    PostMessageW(tray_window, WM_COMMAND, 1004, 0);
    DWORD wait = WaitForSingleObject(process.hProcess, 5000);
    if (wait != WAIT_OBJECT_0) {
        TerminateProcess(process.hProcess, 93);
        return 16;
    }
    DWORD exit_code = 1;
    GetExitCodeProcess(process.hProcess, &exit_code);
    wprintf(L"Tray window found; context popup shown; Quit command exited with code %lu.\n",
            exit_code);
    CloseHandle(trigger);
    CloseHandle(process.hThread);
    CloseHandle(process.hProcess);
    return exit_code == 0 ? 0 : 17;
}
