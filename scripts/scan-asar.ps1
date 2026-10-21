param(
    [string]$Path = "D:\soft\deepseek\resources\app.asar",
    [string[]]$Needles = @(),
    [int]$Before = 300,
    [int]$After = 700,
    [int]$MaxHits = 2
)

$fs = [System.IO.File]::OpenRead($Path)
$buffer = New-Object byte[] 4194304
$offset = 0
$found = @()

foreach ($needle in $Needles) {
    $fs.Position = 0
    $offset = 0
    $count = 0
    while ($count -lt $MaxHits) {
        $read = $fs.Read($buffer, 0, $buffer.Length)
        if ($read -le 0) { break }
        $text = [System.Text.Encoding]::UTF8.GetString($buffer, 0, $read)
        $index = 0
        while ($count -lt $MaxHits -and ($index = $text.IndexOf($needle, $index)) -ge 0) {
            $start = [Math]::Max(0, $index - $Before)
            $length = [Math]::Min($Before + $After, $text.Length - $start)
            $found += "===== $needle @ $($offset + $index) ====="
            $found += $text.Substring($start, $length)
            $found += ""
            $count++
            $index += $needle.Length
        }
        $offset += $read
    }
}

$fs.Close()
if ($found.Count -eq 0) {
    "no hits for: $($Needles -join ', ')"
} else {
    $found -join "`n"
}
