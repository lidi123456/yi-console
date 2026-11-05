param([Parameter(Mandatory=$true)][string]$Root)
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'
$root = $Root
# Run only in the signed-in test user's desktop; all artifacts stay in Root.
Add-Type -AssemblyName System.Windows.Forms
Add-Type -AssemblyName System.Drawing
$form = New-Object System.Windows.Forms.Form
$form.Text = 'Yi Console Computer Use Test'
$form.Size = New-Object System.Drawing.Size(580,350)
$form.StartPosition = 'Manual'
$form.Location = New-Object System.Drawing.Point(70,100)
$label = New-Object System.Windows.Forms.Label
$label.Text = 'Yi Console desktop validation - isolated test window'
$label.Location = New-Object System.Drawing.Point(20,20)
$label.Size = New-Object System.Drawing.Size(520,30)
$form.Controls.Add($label)
$validationText = New-Object System.Windows.Forms.TextBox
$validationText.AccessibleName = 'Validation text'
$validationText.Location = New-Object System.Drawing.Point(20,65)
$validationText.Size = New-Object System.Drawing.Size(420,30)
$form.Controls.Add($validationText)
$button = New-Object System.Windows.Forms.Button
$button.Text = 'Apply test text'
$button.AccessibleName = 'Apply test text'
$button.Location = New-Object System.Drawing.Point(20,110)
$button.Size = New-Object System.Drawing.Size(180,40)
$form.Controls.Add($button)
$output = New-Object System.Windows.Forms.Label
$output.Text = 'Waiting for input'
$output.Location = New-Object System.Drawing.Point(20,175)
$output.Size = New-Object System.Drawing.Size(520,25)
$form.Controls.Add($output)
$codeImage = New-Object System.Windows.Forms.PictureBox
$codeImage.AccessibleName = 'Visual check code (read the screenshot)'
$codeImage.Location = New-Object System.Drawing.Point(20,210)
$codeImage.Size = New-Object System.Drawing.Size(520,40)
$form.Controls.Add($codeImage)
$button.Add_Click({
  $code = (Get-Random -Minimum 100000 -Maximum 999999).ToString()
  $output.Text = 'Verified: ' + $validationText.Text
  # Render the random code into pixels so an accessibility-tree answer cannot pass the visual check.
  $bitmap = New-Object System.Drawing.Bitmap(520,40)
  $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
  $graphics.Clear([System.Drawing.Color]::White)
  $graphics.DrawString(('Check code: ' + $code), $form.Font, [System.Drawing.Brushes]::Black, [single]4, [single]4)
  $graphics.Dispose()
  $previousImage = $codeImage.Image
  $codeImage.Image = $bitmap
  if ($previousImage) { $previousImage.Dispose() }
  @{ value=$validationText.Text; code=$code; pid=$PID; windowId=$form.Handle.ToInt64(); clickedAt=(Get-Date).ToString('o') } | ConvertTo-Json | Set-Content -Encoding UTF8 -Path "$root\fixture-result.json"
})
$form.Font = New-Object System.Drawing.Font('Segoe UI', 10)
$form.Add_Shown({
  if (Test-Path "$root\on-window-ready.ps1") { & "$root\on-window-ready.ps1" }
  Set-Content -Path "$root\fixture-ready.json" -Encoding UTF8 -Value ('{"pid":' + $PID + ',"windowId":' + $form.Handle.ToInt64() + '}')
})
$timer = New-Object System.Windows.Forms.Timer
$timer.Interval = 500
$timer.Add_Tick({ if (Test-Path "$root\fixture.done") { $form.Close() } })
$timer.Start()
[System.Windows.Forms.Application]::Run($form)
if ($codeImage.Image) { $codeImage.Image.Dispose() }
