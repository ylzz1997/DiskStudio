@echo off
node "%~dp0lab.mjs" shutdown
if errorlevel 1 pause
