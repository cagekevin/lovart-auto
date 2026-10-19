@echo off
chcp 65001 >nul
setlocal EnableExtensions
rem `%~dp0` 末尾带反斜杠，直接 `cd /d "…\"` 在部分 cmd 版本会被当成转义引号；
rem 用 `pushd`（对末尾反斜杠容忍）最稳。
pushd "%~dp0"
title lovart-auto - 一键打包 Windows

echo ============================================================
echo   lovart-auto 一键打包（Windows 单文件 exe）
echo   产物：%~dp0lovart-client.exe
echo ============================================================
echo.

rem ── 1. 占用检测 ────────────────────────────────────────────────
rem 覆盖 exe 前必须先退出程序；这里**不自动杀进程** —— 那会中断正在跑的任务。
rem 用绝对路径调 tasklist/find，避免 PATH 里的同名工具（Git 自带 find.exe）抢先。
:check_running
"%SystemRoot%\System32\tasklist.exe" /FI "IMAGENAME eq lovart-client.exe" /NH 2>nul | "%SystemRoot%\System32\find.exe" /I "lovart-client.exe" >nul
if errorlevel 1 goto build
echo [!] 检测到 lovart-client.exe 正在运行。
echo     打包需要覆盖它，请先关闭程序（正在跑的任务不会被中断）。
echo.
if defined LOVART_NO_PAUSE goto running_fail
echo     关闭后按任意键重试；直接关掉本窗口 = 取消。
pause >nul
echo.
goto check_running

:running_fail
echo [x] 程序仍在运行，已取消打包。
exit /b 2

rem ── 2. 调用唯一的打包实现 ──────────────────────────────────────
rem 不在 bat 里重写构建步骤：make-exe.ps1 是 Windows 打包的唯一入口
rem （图标 → MSVC 环境 → cargo build --release → 拷到仓库根）。
:build
echo [1/2] 编译并组装（首次约 2 分钟，之后更快）...
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\make-exe.ps1" %*
if errorlevel 1 goto fail

echo.
echo [2/2] 完成
if exist "%~dp0lovart-client.exe" (
  for %%F in ("%~dp0lovart-client.exe") do echo     产物：%%~fF
)
echo.
echo     双击 exe = 图形界面
echo     命令行   = lovart-client.exe --help
echo.
if not defined LOVART_NO_PAUSE (
  echo 按任意键关闭窗口...
  pause >nul
)
exit /b 0

:fail
echo.
echo [x] 打包失败，请查看上面的错误信息。
if not defined LOVART_NO_PAUSE (
  echo.
  echo 按任意键关闭窗口...
  pause >nul
)
exit /b 1
