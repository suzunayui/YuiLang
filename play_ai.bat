@echo off
rem AI の仲間と遊ぶゲーム（games\blocks_ai.yui）を、ビルドしてから起動する。
rem ダブルクリックでも、ターミナルからでも使える。引数はそのままゲームに渡る。
chcp 65001 >nul
setlocal
cd /d "%~dp0"

rem yui 本体がまだなければ作る（最初の 1 回だけ、数十秒かかる）
if not exist "target\release\yui.exe" (
  echo yui 本体をビルドしています...
  cargo build --release
  if errorlevel 1 (
    echo yui 本体のビルドに失敗しました。
    pause
    exit /b 1
  )
)

echo ゲームをビルドしています...
"target\release\yui.exe" build games\blocks_ai.yui -o blocks_ai.exe
if errorlevel 1 (
  echo ゲームのビルドに失敗しました。
  pause
  exit /b 1
)

if "%ANTHROPIC_API_KEY%"=="" (
  echo.
  echo 注意: 環境変数 ANTHROPIC_API_KEY が見つかりません。
  echo       ユイは決まった返事（木集め・おうち作り）だけをします。
  echo.
)

"%~dp0blocks_ai.exe" %*
if errorlevel 1 pause
