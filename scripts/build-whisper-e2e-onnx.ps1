#Requires -Version 7.0
<#
.SYNOPSIS
    Builds, validates, and pins the optional end-to-end Whisper ONNX artifact.

.DESCRIPTION
    Synapse feeds raw audio container bytes to `audio_stream` and reads decoded
    text from `str`. Microsoft does not publish this fused graph. This recipe
    reproduces the final maintained Olive Whisper workflow at commit
    6ab9d8bb9284a89ccc87c63ca323c0f3386f6426 (Olive v0.8.0):

      PyTorch components -> ONNX -> transformer optimization -> component INT8
      quantization -> BeamSearch insertion -> audio/text pre/post processing.

    Ordering is binding. Quantizing after BeamSearch corrupts contrib-domain
    subgraphs. Transformers is pinned below 4.43 because Microsoft's recipe
    records that later versions export graphs which decode to empty text.

    The upstream recipe files are downloaded by immutable commit and checked
    against committed SHA-256 values. The candidate is not published or pinned
    until graph shape, ORT Extensions loading, and a known real WAV transcript
    all pass. There is no fallback artifact.
#>
[CmdletBinding()]
param(
    [string]$SourceDir = (Split-Path -Parent $PSScriptRoot),
    [string]$OutputPath = (Join-Path $env:LOCALAPPDATA 'synapse\models\whisper-tiny-int8.onnx'),
    [string]$WorkRoot = (Join-Path $env:LOCALAPPDATA 'synapse\build-whisper'),
    [string]$Model = 'openai/whisper-tiny.en',
    [switch]$NoRepin
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Info($message) { Write-Host "[build-whisper] $message" }
function Die($message) {
    Write-Host "[build-whisper] FATAL: $message" -ForegroundColor Red
    exit 1
}

$pythonPackages = @(
    'torch==2.4.1',
    'transformers==4.42.4',
    'olive-ai==0.8.0',
    'onnx==1.17.0',
    'onnxruntime==1.19.2',
    'onnxruntime-extensions==0.12.0',
    'numpy==1.26.4',
    'librosa==0.10.2.post1',
    'datasets==3.2.0'
)

$oliveCommit = '6ab9d8bb9284a89ccc87c63ca323c0f3386f6426'
$oliveWhisperFiles = [ordered]@{
    'prepare_whisper_configs.py'           = '4AF42A6F200433A351556D23FF946D15874982F3E7D476B0513A0FD21207C87C'
    'test_transcription.py'                = '20876ECC3CB7C89D57DE68000CC832189400DE8EF1199B8186E8389E65BA4CC2'
    'whisper_template.json'                = '0F0806DB5C1BDCD152E37D8F75F31E29E441BCE72908F5F5C12C9CD65975C716'
    'code/past_helper.py'                  = 'D5A8BB36148A374EF6D6EB91997871AF6A25EF1F0CDD5E58A8D9DB134540AC54'
    'code/user_script.py'                  = '5CA7959F44F42287912FB32C0289AA1D18AAA1F27DFF9181EC422C247DBC9202'
    'code/whisper_dataset.py'              = '5ABF2333257F6E334DBE87E057D35919D415C1EECFB8866520E7298A18864F86'
    'code/whisper_decoder.py'              = '0270EA0DDD6B68D0761A7CB281EC5767C09981A31458E0AB16FA2A5C2C9C25AE'
    'code/whisper_encoder.py'              = '0C6B368B8A8146501AAEFD96D4AE0A18451B487BBB99ED6E2EEA3C0A231B1AD1'
    'code/whisper_encoder_decoder_init.py' = '4D926F20CA9D8DC3B5175244B1ACCF220C0E4FA95FE2D2D7E8E1EF064E3D722D'
}

$resolvedSource = [System.IO.Path]::GetFullPath($SourceDir)
if (-not (Test-Path -LiteralPath $resolvedSource -PathType Container)) {
    Die "SYNAPSE_WHISPER_BUILD_SOURCE_DIR_MISSING path=$resolvedSource remediation=pass -SourceDir pointing at the Synapse checkout"
}
$pinPath = Join-Path $resolvedSource 'models\whisper-tiny-int8.pin.json'
$registryPath = Join-Path $resolvedSource 'crates\synapse-models\src\registry.rs'
foreach ($required in @($pinPath, $registryPath)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        Die "SYNAPSE_WHISPER_BUILD_PIN_TARGET_MISSING path=$required remediation=restore the missing repository pin target before building"
    }
}

$python = Get-Command python -ErrorAction SilentlyContinue
if (-not $python) { $python = Get-Command python3 -ErrorAction SilentlyContinue }
if (-not $python) {
    Die 'SYNAPSE_WHISPER_BUILD_PYTHON_MISSING remediation=install CPython 3.9-3.12 and ensure python resolves on PATH'
}
Info "Python: $($python.Source) ($((& $python.Source --version 2>&1) -join ''))"

New-Item -ItemType Directory -Force -Path $WorkRoot | Out-Null
$venvDir = Join-Path $WorkRoot 'venv'
$venvPython = Join-Path $venvDir 'Scripts\python.exe'
if (-not (Test-Path -LiteralPath $venvPython -PathType Leaf)) {
    Info "Creating isolated environment -> $venvDir"
    & $python.Source -m venv $venvDir
    if ($LASTEXITCODE -ne 0) {
        Die "SYNAPSE_WHISPER_BUILD_VENV_FAILED path=$venvDir exit_code=$LASTEXITCODE remediation=repair the CPython venv module and retry"
    }
}
if (-not (Test-Path -LiteralPath $venvPython -PathType Leaf)) {
    Die "SYNAPSE_WHISPER_BUILD_VENV_INCOMPLETE path=$venvPython remediation=delete only this incomplete build venv and retry"
}

Info "Installing pinned toolchain: $($pythonPackages -join ', ')"
& $venvPython -m pip install --disable-pip-version-check --no-input --upgrade pip
if ($LASTEXITCODE -ne 0) {
    Die "SYNAPSE_WHISPER_BUILD_PIP_UPGRADE_FAILED exit_code=$LASTEXITCODE remediation=inspect network access to PyPI"
}
& $venvPython -m pip install --disable-pip-version-check --no-input @pythonPackages
if ($LASTEXITCODE -ne 0) {
    Die "SYNAPSE_WHISPER_BUILD_PIP_INSTALL_FAILED exit_code=$LASTEXITCODE remediation=repair the pinned Python package installation; no model or pin was changed"
}

$recipeRoot = Join-Path $WorkRoot 'olive-v0.8-whisper'
New-Item -ItemType Directory -Force -Path $recipeRoot | Out-Null
$rawBase = "https://raw.githubusercontent.com/microsoft/Olive/$oliveCommit/examples/whisper"
foreach ($entry in $oliveWhisperFiles.GetEnumerator()) {
    $relative = [string]$entry.Key
    $expected = [string]$entry.Value
    $destination = Join-Path $recipeRoot ($relative.Replace('/', '\'))
    $parent = Split-Path -Parent $destination
    New-Item -ItemType Directory -Force -Path $parent | Out-Null
    $validExisting = (Test-Path -LiteralPath $destination -PathType Leaf) -and
        ((Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash -eq $expected)
    if (-not $validExisting) {
        $candidate = "$destination.download-$PID"
        try {
            Invoke-WebRequest -Uri "$rawBase/$relative" -OutFile $candidate
            $observed = (Get-FileHash -LiteralPath $candidate -Algorithm SHA256).Hash
            if ($observed -ne $expected) {
                Die "SYNAPSE_WHISPER_BUILD_UPSTREAM_HASH_MISMATCH file=$relative expected=$expected observed=$observed commit=$oliveCommit remediation=do not use these bytes; inspect the immutable upstream commit and network path"
            }
            [System.IO.File]::Copy($candidate, $destination, $true)
        } finally {
            if (Test-Path -LiteralPath $candidate) { Remove-Item -LiteralPath $candidate -Force }
        }
    }
    $readback = (Get-FileHash -LiteralPath $destination -Algorithm SHA256).Hash
    if ($readback -ne $expected) {
        Die "SYNAPSE_WHISPER_BUILD_RECIPE_READBACK_MISMATCH file=$relative expected=$expected observed=$readback remediation=repair the external build workspace and retry"
    }
}
Info "Verified Olive Whisper recipe commit=$oliveCommit files=$($oliveWhisperFiles.Count)"

Push-Location $recipeRoot
try {
    & $venvPython prepare_whisper_configs.py --model_name $Model --skip_evaluation --multilingual
    if ($LASTEXITCODE -ne 0) {
        Die "SYNAPSE_WHISPER_BUILD_CONFIG_FAILED exit_code=$LASTEXITCODE model=$Model remediation=inspect the pinned Olive config-generation output; no model or pin was changed"
    }
    Info 'Running Olive CPU INT8 workflow (component quantization precedes BeamSearch)'
    & $venvPython -m olive run --config whisper_cpu_int8.json
    if ($LASTEXITCODE -ne 0) {
        Die "SYNAPSE_WHISPER_BUILD_OLIVE_FAILED exit_code=$LASTEXITCODE model=$Model remediation=inspect the Olive pass output; no model or pin was changed"
    }
} finally {
    Pop-Location
}

$candidateModel = Join-Path $recipeRoot 'models\whisper_cpu_int8\model.onnx'
if (-not (Test-Path -LiteralPath $candidateModel -PathType Leaf)) {
    Die "SYNAPSE_WHISPER_BUILD_ARTIFACT_MISSING path=$candidateModel remediation=Olive returned success without publishing its declared model; no pin was changed"
}

$knownWav = Join-Path $WorkRoot 'known-quick-brown-fox.wav'
try {
    Add-Type -AssemblyName System.Speech
    $speech = [System.Speech.Synthesis.SpeechSynthesizer]::new()
    try {
        $speech.SetOutputToWaveFile($knownWav)
        $speech.Speak('The quick brown fox jumps over the lazy dog.')
    } finally {
        $speech.Dispose()
    }
} catch {
    Die "SYNAPSE_WHISPER_BUILD_KNOWN_AUDIO_FAILED error=$($_.Exception.Message) remediation=repair the Windows System.Speech installation; behavioral parity is mandatory before pinning"
}
if (-not (Test-Path -LiteralPath $knownWav -PathType Leaf)) {
    Die "SYNAPSE_WHISPER_BUILD_KNOWN_AUDIO_MISSING path=$knownWav remediation=repair Windows speech synthesis and retry"
}
$knownAudioSha = (Get-FileHash -LiteralPath $knownWav -Algorithm SHA256).Hash

$verifyScript = Join-Path $WorkRoot 'verify_whisper_e2e.py'
$verifySource = @'
import os
import sys
import numpy as np
import onnx
import onnxruntime as ort
from onnxruntime_extensions import get_library_path

model_path, audio_path = sys.argv[1:3]
model = onnx.load(model_path, load_external_data=False)
inputs = {item.name for item in model.graph.input}
outputs = {item.name for item in model.graph.output}
required = {
    "audio_stream", "max_length", "min_length", "num_beams",
    "num_return_sequences", "length_penalty", "repetition_penalty",
    "decoder_input_ids",
}
if inputs != required:
    raise RuntimeError(f"input contract mismatch required={sorted(required)} observed={sorted(inputs)}")
if outputs != {"str"}:
    raise RuntimeError(f"output contract mismatch required=['str'] observed={sorted(outputs)}")

options = ort.SessionOptions()
options.register_custom_ops_library(get_library_path())
session = ort.InferenceSession(model_path, sess_options=options, providers=["CPUExecutionProvider"])
with open(audio_path, "rb") as stream:
    audio = np.asarray(list(stream.read()), dtype=np.uint8)[None, :]
feed = {
    "audio_stream": audio,
    "max_length": np.asarray([96], dtype=np.int32),
    "min_length": np.asarray([0], dtype=np.int32),
    "num_beams": np.asarray([1], dtype=np.int32),
    "num_return_sequences": np.asarray([1], dtype=np.int32),
    "length_penalty": np.asarray([1.0], dtype=np.float32),
    "repetition_penalty": np.asarray([1.0], dtype=np.float32),
    "decoder_input_ids": np.asarray([[50257, 50362]], dtype=np.int32),
}
text = str(session.run(["str"], feed)[0].reshape(-1)[0]).strip()
expected = "The quick brown fox jumps over the lazy dog."
if text != expected:
    raise RuntimeError(f"behavioral parity failed expected={expected!r} observed={text!r}")
print(f"verified_transcript={text}")
print(f"model_bytes={os.path.getsize(model_path)}")
'@
Set-Content -LiteralPath $verifyScript -Value $verifySource -Encoding UTF8
& $venvPython $verifyScript $candidateModel $knownWav
if ($LASTEXITCODE -ne 0) {
    Die "SYNAPSE_WHISPER_BUILD_BEHAVIOR_FAILED exit_code=$LASTEXITCODE known_audio_sha256=$knownAudioSha remediation=do not publish or pin this graph; inspect version compatibility and Olive pass output"
}
Info "Behavioral parity passed known_audio_sha256=$knownAudioSha"

$outputDirectory = Split-Path -Parent ([System.IO.Path]::GetFullPath($OutputPath))
New-Item -ItemType Directory -Force -Path $outputDirectory | Out-Null
[System.IO.File]::Copy($candidateModel, $OutputPath, $true)
if (-not (Test-Path -LiteralPath $OutputPath -PathType Leaf)) {
    Die "SYNAPSE_WHISPER_BUILD_PUBLISH_MISSING path=$OutputPath remediation=the verified candidate copy did not persist; no pin was changed"
}
$length = [int64](Get-Item -LiteralPath $OutputPath).Length
$sha = (Get-FileHash -LiteralPath $OutputPath -Algorithm SHA256).Hash.ToUpperInvariant()
Info "Produced verified artifact path=$OutputPath length=$length sha256=$sha"
$extensionsSource = & $venvPython -c 'from onnxruntime_extensions import get_library_path; print(get_library_path())'
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace($extensionsSource) -or -not (Test-Path -LiteralPath $extensionsSource.Trim() -PathType Leaf)) {
    Die 'SYNAPSE_WHISPER_BUILD_EXTENSIONS_LIBRARY_MISSING source_of_truth=onnxruntime_extensions.get_library_path remediation=repair the pinned Python environment and rebuild'
}
$extensionsTarget = Join-Path (Split-Path -Parent $OutputPath) 'ort-extensions\onnxruntime_extensions.dll'
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $extensionsTarget) | Out-Null
Copy-Item -LiteralPath $extensionsSource.Trim() -Destination $extensionsTarget -Force
$extensionsSha = (Get-FileHash -LiteralPath $extensionsTarget -Algorithm SHA256).Hash.ToUpperInvariant()
$extensionsLength = [int64](Get-Item -LiteralPath $extensionsTarget).Length
if ($extensionsSha -ne '0A0ACEA84AC7D90E5B6E81A6C37E19B8C356BD19809D44839EF945B3F2FEE353' -or $extensionsLength -ne 3333664) {
    Die "SYNAPSE_WHISPER_BUILD_EXTENSIONS_IDENTITY_MISMATCH path=$extensionsTarget expected_sha256=0A0ACEA84AC7D90E5B6E81A6C37E19B8C356BD19809D44839EF945B3F2FEE353 actual_sha256=$extensionsSha expected_length=3333664 actual_length=$extensionsLength remediation=do not deploy an unpinned Extensions wheel"
}
Info "Provisioned pinned ONNX Runtime Extensions library path=$extensionsTarget length=$extensionsLength sha256=$extensionsSha"

if ($NoRepin) {
    Info "-NoRepin: committed pins unchanged. Candidate sha256=$sha length=$length"
    exit 0
}

$pin = Get-Content -LiteralPath $pinPath -Raw -Encoding UTF8 | ConvertFrom-Json
$previousSha = [string]$pin.sha256
$pin.sha256 = $sha
$pin.length = $length
$pin.provenance.pinned_by = "Microsoft Olive v0.8.0 commit $oliveCommit from $Model; graph and known-WAV parity verified"
$pin.provenance.note = "Reproducible local build via the committed recipe. Component INT8 quantization precedes BeamSearch insertion; Transformers 4.42.4 is required because >=4.43 exports empty-decoding Whisper graphs."
$pin | ConvertTo-Json -Depth 32 | Set-Content -LiteralPath $pinPath -Encoding UTF8
Info "Rewrote installer pin $pinPath ($previousSha -> $sha)"

$registryText = Get-Content -LiteralPath $registryPath -Raw -Encoding UTF8
$pattern = '(WHISPER_TINY_INT8_ONNX_SHA256\s*:\s*&str\s*=\s*)"sha256:[0-9a-fA-F]{64}"'
if (-not [regex]::IsMatch($registryText, $pattern)) {
    Die "SYNAPSE_WHISPER_BUILD_REPIN_CONSTANT_NOT_FOUND path=$registryPath remediation=revert the pin-file change or update the missing daemon constant before building"
}
$registryText = [regex]::Replace($registryText, $pattern, ('${1}"sha256:' + $sha.ToLowerInvariant() + '"'))
$lengthPattern = '(WHISPER_TINY_INT8_ONNX_LENGTH\s*:\s*u64\s*=\s*)[0-9_]+'
if (-not [regex]::IsMatch($registryText, $lengthPattern)) {
    Die "SYNAPSE_WHISPER_BUILD_REPIN_LENGTH_CONSTANT_NOT_FOUND path=$registryPath remediation=revert the pin-file change or restore the single runtime length constant before building"
}
$registryText = [regex]::Replace($registryText, $lengthPattern, ('${1}' + $length.ToString('N0', [Globalization.CultureInfo]::InvariantCulture).Replace(',', '_')))
Set-Content -LiteralPath $registryPath -Value $registryText -Encoding UTF8 -NoNewline
Info 'Rewrote daemon model SHA-256 and byte-length constants'
Info "Deploy with: `$env:SYNAPSE_WHISPER_ONNX_SOURCE='$OutputPath'; scripts\synapse-setup.ps1 -SourceDir '$resolvedSource'"
