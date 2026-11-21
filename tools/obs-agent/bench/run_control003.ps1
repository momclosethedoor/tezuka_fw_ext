param(
    [Parameter(Mandatory=$true)][ValidateSet('repeat','maps','tx-map','protection','modes','ack-last','ack-exhaust','ack4-stop','ack4-exhaust','take','take-stop','progress','close-baseline','close-next','close-next-fault','close-cycle')][string]$Case,
    [Parameter(Mandatory=$true)][ValidatePattern('^[A-Za-z0-9_-]+$')][string]$Run,
    [Parameter(Mandatory=$true)][string]$Agent,
    [Parameter(Mandatory=$true)][string]$Modules,
    [ValidateRange(1,5)][int]$Repeat=1,
    [string]$Fault='',
    [switch]$NoTrace,
    [switch]$Close
)
$ErrorActionPreference='Stop'
$fw=(Resolve-Path "$PSScriptRoot/../../..").Path
$workspace=Split-Path $fw
$old="$workspace/evidence/ARM-SCAN-CONTROL-002"
$previous="$workspace/evidence/ARM-SCAN-027-ACCEPT-001"
$root="$workspace/evidence/ARM-SCAN-CONTROL-003"
if ($Close -or $Case.StartsWith('close-')) { $root += '-CLOSE' }
$Agent=(Resolve-Path $Agent).Path
$Modules=(Resolve-Path $Modules).Path
$py=Join-Path (Split-Path $workspace) 'goofy_scan/.venv/Scripts/python.exe'
New-Item -ItemType Directory -Path $root -Force | Out-Null
if (Test-Path -LiteralPath "$root/$Run") { throw "Existing run $root/$Run" }
$fixture=@{
    PLANNER_BENCH_XTG1="$previous/tpc2/ram/system_top.bit.bin"
    PLANNER_BENCH_MODULES=$Modules
    PLANNER_BENCH_GEOMETRY='27/9'
    PLANNER_BENCH_PATH_ACK='1'
    PLANNER_BENCH_VERIFY_HANDBACK='1'
    PLANNER_BENCH_ROUTE='mixer'
    PLANNER_BENCH_GATE_TICKS='256'
    PLANNER_BENCH_TPC2_DIAGNOSTIC='1'
    PLANNER_BENCH_LEASE_SCRIPT="$fw/board/tezuka/nano/overlay_goofy/usr/sbin/goofy_lease.sh"
    PLANNER_BENCH_BOARD_SAMPLES='1'
    PLANNER_BENCH_OWNER_SECONDS='225'
    PLANNER_BENCH_REPEAT=[string]$Repeat
    PLANNER_BENCH_CONTROL_TRACE=''
    PLANNER_BENCH_PHASE_TIMING=''
    PLANNER_BENCH_SWEEP="$old/short-managed.json"
}
if (!$NoTrace) { $fixture.PLANNER_BENCH_CONTROL_TRACE='1'; $fixture.PLANNER_BENCH_PHASE_TIMING='1' }
switch ($Case) {
    {$_ -in 'close-baseline','close-next','close-next-fault','close-cycle'} {
        $fixture.PLANNER_BENCH_ACK3_FAST='1'
        if ($Case -eq 'close-next-fault' -or $Case -eq 'close-cycle') {
            $fixture.PLANNER_BENCH_SWEEP="$PSScriptRoot/control003-close-cycle.json"
        } else {
            $fixture.PLANNER_BENCH_SWEEP="$PSScriptRoot/control003-maps.json"
            $fixture.PLANNER_BENCH_CASE_INDEX='8'
        }
        if ($Case -eq 'close-baseline') {
            $fixture.PLANNER_BENCH_NEXT_BASELINE='1'
        } else {
            $fixture.PLANNER_BENCH_CLOSE_CLOCK='1'
            $fixture.PLANNER_BENCH_NEXT_CLOSE='hold'
        }
        if ($Case -eq 'close-next-fault') {
            $fixture.PLANNER_BENCH_CASE_INDEX='2'
            $fixture.PLANNER_BENCH_NEXT_CLOSE='fault'
        }
        if ($Case -eq 'close-cycle') {
            $fixture.PLANNER_BENCH_CATALOG='1'
            $fixture.PLANNER_BENCH_CLOSE_CYCLE='1'
            $fixture.PLANNER_BENCH_CLOSE_BANK='1'
            $fixture.PLANNER_BENCH_NEXT_CLOSE='check'
            $fixture.PLANNER_BENCH_MIN_SECONDS='3'
        }
    }
    'repeat' { $fixture.PLANNER_BENCH_SWEEP="$previous/tpc2-rf.json"; $fixture.PLANNER_BENCH_MIN_SECONDS='1' }
    'maps' {
        $fixture.PLANNER_BENCH_SWEEP="$PSScriptRoot/control003-maps.json"
        $fixture.PLANNER_BENCH_CATALOG='1'
        $fixture.PLANNER_BENCH_ACK3_FAST='1'
        $fixture.PLANNER_BENCH_SOAK_CASE='1,2'
        $fixture.PLANNER_BENCH_SOAK_SECONDS='60'
    }
    'tx-map' {
        $fixture.PLANNER_BENCH_SWEEP="$PSScriptRoot/control003-maps.json"
        $fixture.PLANNER_BENCH_CASE_INDEX='4'
        $fixture.PLANNER_BENCH_ACK3_FAST='1'
    }
    'protection' {
        $fixture.PLANNER_BENCH_SWEEP="$old/protection-cases.json"
        $fixture.PLANNER_BENCH_LEASE_LOSS='1'
        $fixture.PLANNER_BENCH_WATCHDOG='2'
        $fixture.PLANNER_BENCH_RESTART_AFTER_LOSS='1'
        $fixture.PLANNER_BENCH_ACK3_FAST='1'
    }
    'modes' { $fixture.PLANNER_BENCH_SWEEP="$old/modes-cases.json"; $fixture.PLANNER_BENCH_ACK3_FAST='1' }
    'progress' { $fixture.PLANNER_BENCH_PROGRESS='1'; $fixture.PLANNER_BENCH_ACK3_FAST='1' }
    'ack-last' {
        $fixture.PLANNER_BENCH_ACK3_FAST='1'
        $fixture.PLANNER_BENCH_ACK_DROP_FIRST='4'
        $fixture.PLANNER_BENCH_ACK_DROP_KIND='3'
        $fixture.PLANNER_BENCH_ACK_LATE_FIRST='1'
    }
    'ack-exhaust' {
        $fixture.PLANNER_BENCH_ACK3_FAST='1'
        $fixture.PLANNER_BENCH_ACK_DROP_FIRST='5'
        $fixture.PLANNER_BENCH_ACK_DROP_KIND='3'
    }
    {$_ -in 'ack4-stop','ack4-exhaust'} {
        $fixture.PLANNER_BENCH_SWEEP="$previous/tpc2-rf.json"
        $fixture.PLANNER_BENCH_CASE_INDEX='4'
        $fixture.PLANNER_BENCH_ACK3_FAST='1'
        $fixture.PLANNER_BENCH_WRONG_PATH_ACK='1'
        if ($Case -eq 'ack4-stop') {
            $fixture.PLANNER_BENCH_STOP_WHILE_ROUTE='1'
            $fixture.PLANNER_BENCH_ACK_DROP_FIRST='1'
            $fixture.PLANNER_BENCH_ACK_DROP_KIND='3'
            $fixture.PLANNER_BENCH_ACK_LATE_FIRST='1'
        }
        else { $fixture.PLANNER_BENCH_MISSING_ACK4='1' }
    }
    {$_ -in 'take','take-stop'} {
        if (!$Fault) { throw 'Specify -Fault stage,delay_ms,attempt_count,fail,cleanup_fail' }
        $fixture.PLANNER_BENCH_TAKE_FAULT=$Fault
        $fixture.PLANNER_BENCH_ACK3_FAST='1'
        if ($Case -eq 'take-stop') { $fixture.PLANNER_BENCH_STOP_DURING_TAKE='1' }
        if ($Fault.EndsWith(',1')) {
            $fixture.PLANNER_BENCH_VERIFY_HANDBACK=''
            $fixture.PLANNER_BENCH_EXPECT_UNSAFE='1'
        }
    }
}
$saved=@{}
try {
    foreach ($key in $fixture.Keys) {
        $saved[$key]=[Environment]::GetEnvironmentVariable($key,'Process')
        [Environment]::SetEnvironmentVariable($key,$fixture[$key],'Process')
    }
    & $py -B "$PSScriptRoot/planner_control.py" "$root/$Run" --agent $Agent --host-ip 192.168.2.10
    $result=$LASTEXITCODE
} finally {
    foreach ($key in $saved.Keys) { [Environment]::SetEnvironmentVariable($key,$saved[$key],'Process') }
}
exit $result
