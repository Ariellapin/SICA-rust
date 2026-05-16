# Wrapper that sets up PATH for cargo + the LLVM-MinGW toolchain, then forwards
# arguments to either `cargo` (default) or another command.
#
# Examples:
#   .\run.ps1 build --workspace
#   .\run.ps1 test --workspace
#   .\run.ps1 run -p frontend
#   .\run.ps1 cmd backend --ipc \\.\pipe\test
#
# The first arg is the cargo subcommand by default. Use `cmd <exe> <args...>`
# to invoke any other binary with the correct PATH.

$cargoBin = Join-Path $env:USERPROFILE ".cargo\bin"
$mingwBin = Join-Path $env:LOCALAPPDATA "Microsoft\WinGet\Packages\MartinStorsjo.LLVM-MinGW.UCRT_Microsoft.Winget.Source_8wekyb3d8bbwe\llvm-mingw-20260505-ucrt-x86_64\bin"

if (-not (Test-Path $cargoBin)) {
    Write-Error "cargo bin not found at $cargoBin. Install Rust via rustup."
    exit 1
}
if (-not (Test-Path $mingwBin)) {
    Write-Error "LLVM-MinGW not found at $mingwBin. Install: winget install MartinStorsjo.LLVM-MinGW.UCRT"
    exit 1
}

$env:Path = "$cargoBin;$mingwBin;$env:Path"

# Run native commands using cmd.exe so PowerShell does not wrap stderr lines
# as ErrorRecord objects (which makes cargo's progress output look like errors).
if ($args.Count -ge 1 -and $args[0] -eq 'cmd') {
    $rest = $args[1..($args.Count - 1)]
    & $rest[0] @($rest[1..($rest.Count - 1)])
    exit $LASTEXITCODE
}

& cargo.exe @args
exit $LASTEXITCODE
