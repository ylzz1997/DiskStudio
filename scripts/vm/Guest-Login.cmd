@echo off
node "%~dp0lab.mjs" login-keys
if errorlevel 1 pause
