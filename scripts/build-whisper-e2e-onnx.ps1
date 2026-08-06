#Requires -Version 7.0
<#
.SYNOPSIS
    Produces the optional end-to-end Whisper ONNX artifact and re-pins it (#1863).

.DESCRIPTION
    Synapse's speech-to-text runtime feeds raw audio container bytes to a single
    ONNX graph on the `audio_stream` input and reads decoded text from the `str`
    output. Audio decoding, log-mel extraction, beam search and BPE detokenization
    are all fused into that one graph using ONNX Runtime Extensions custom
    operators.

    No public repository publishes that artifact. Every HuggingFace
    `whisper-tiny` ONNX repository (onnx-community, Xenova, optimum, sherpa)
    ships the split encoder/decoder export, which takes `input_features` and
    emits token ids -- it does NOT satisfy this contract and cannot be
    substituted. The artifact must therefore be produced from the upstream
    recipe, which is what this script does.

    The previous pin (`bundled://synapse/whisper-tiny-int8`) named bytes that
    existed only in one build host's %LOCALAPPDATA% and were lost, which made a
    clean install impossible. This script is the replacement acquisition path.

    On success it:
      1. writes the artifact,
      2. prints its exact byte length and SHA-256,
      3. rewrites models\whisper-tiny-int8.pin.json, and
      4. rewrites WHISPER_TINY_INT8_ONNX_SHA256 in
         crates\synapse-models\src\registry.rs
    so the installer pin and the compiled daemon constant cannot drift. Commit
    the result as a reviewable supply-chain change, then run:

        $env:SYNAPSE_WHISPER_ONNX_SOURCE = '<produced .onnx path>'
        scripts\synapse-setup.ps1 -SourceDir <checkout>

.NOTES
    This is a BUILD-HOST operation, not part of installation. Audio is disabled
    by default (--enable-audio); a Synapse install without this artifact is
    fully supported and reports speech-to-text as unavailable.

    It downloads a multi-gigabyte PyTorch toolchain into an isolated virtual
    environment. Nothing is written inside the repository checkout except the
    two pin files above.
#>
[CmdletBinding()]
param(
    # Checkout whose pin files are rewritten on success.
    [string]$SourceDir = (Split-Path -Parent $PSScriptRoot),

    # Where the produced .onnx is written.
    [string]$OutputPath = (Join-Path $env:LOCALAPPDATA 'synapse\models\whisper-tiny-int8.onnx'),

    # Isolated Python environment root. Deliberately outside the checkout.
    [string]$WorkRoot = (Join-Path $env:LOCALAPPDATA 'synapse\build-whisper'),

    # Upstream model to quantize.
    [string]$Model = 'openai/whisper-tiny.en',

    # Produce the artifact but leave the committed pins untouched.
    [switch]$NoRepin
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

function Info($m) { Write-Host "[build-whisper] $m" }
function Die($m) {
    Write-Host "[build-whisper] FATAL: $m" -ForegroundColor Red
    exit 1
}

# Pinned so a rebuild does not silently pick up a different quantizer and
# produce different bytes for reasons nobody can reconstruct later.
$pythonPackages = @(
    'torch==2.4.1',
    'transformers==4.44.2',
    'onnx==1.16.2',
    'onnxruntime==1.19.2',
    'onnxruntime-extensions==0.12.0',
    'numpy==1.26.4',
    'librosa==0.10.2.post1',
    # The ORT Whisper parity check imports this and otherwise invokes an
    # unqualified `pip install datasets`, which can mutate the build host's
    # global interpreter instead of the isolated environment.
    'datasets==3.2.0'
)

$resolvedSource = [System.IO.Path]::GetFullPath($SourceDir)
if (-not (Test-Path -LiteralPath $resolvedSource -PathType Container)) {
    Die "SYNAPSE_WHISPER_BUILD_SOURCE_DIR_MISSING path=$resolvedSource remediation=pass -SourceDir pointing at the Synapse checkout"
}
$pinPath = Join-Path $resolvedSource 'models\whisper-tiny-int8.pin.json'
$registryPath = Join-Path $resolvedSource 'crates\synapse-models\src\registry.rs'
foreach ($required in @($pinPath, $registryPath)) {
    if (-not (Test-Path -LiteralPath $required -PathType Leaf)) {
        Die "SYNAPSE_WHISPER_BUILD_PIN_TARGET_MISSING path=$required remediation=this script re-pins both the installer pin file and the daemon constant; both must exist"
    }
}

$python = (Get-Command python -ErrorAction SilentlyContinue)
if (-not $python) { $python = (Get-Command python3 -ErrorAction SilentlyContinue) }
if (-not $python) {
    Die "SYNAPSE_WHISPER_BUILD_PYTHON_MISSING remediation=install CPython 3.9-3.12 and ensure `python` resolves on PATH"
}
$pythonVersion = (& $python.Source --version 2>&1) -join ''
Info "Python: $($python.Source) ($pythonVersion)"

New-Item -ItemType Directory -Force -Path $WorkRoot | Out-Null
$venvDir = Join-Path $WorkRoot 'venv'
$venvPython = Join-Path $venvDir 'Scripts\python.exe'
if (-not (Test-Path -LiteralPath $venvPython -PathType Leaf)) {
    Info "Creating isolated environment -> $venvDir"
    & $python.Source -m venv $venvDir
    if ($LASTEXITCODE -ne 0) {
        Die "SYNAPSE_WHISPER_BUILD_VENV_FAILED path=$venvDir exit_code=$LASTEXITCODE remediation=inspect the Python installation; the venv module must be available"
    }
}
if (-not (Test-Path -LiteralPath $venvPython -PathType Leaf)) {
    Die "SYNAPSE_WHISPER_BUILD_VENV_INCOMPLETE path=$venvPython remediation=the virtual environment was created but has no interpreter; delete $venvDir and retry"
}

Info "Installing pinned toolchain: $($pythonPackages -join ', ')"
& $venvPython -m pip install --disable-pip-version-check --no-input --upgrade pip
if ($LASTEXITCODE -ne 0) {
    Die "SYNAPSE_WHISPER_BUILD_PIP_UPGRADE_FAILED exit_code=$LASTEXITCODE remediation=inspect network access to PyPI"
}
& $venvPython -m pip install --disable-pip-version-check --no-input @pythonPackages
if ($LASTEXITCODE -ne 0) {
    Die "SYNAPSE_WHISPER_BUILD_PIP_INSTALL_FAILED exit_code=$LASTEXITCODE packages=$($pythonPackages -join ',') remediation=the pinned toolchain could not be installed; check the interpreter version supports these pins and that PyPI is reachable"
}

$exportScript = Join-Path $WorkRoot 'export_whisper_e2e.py'
$exportSource = @'
"""Builds the ONNX Runtime Extensions end-to-end Whisper graph.

Mirrors onnxruntime-extensions/tutorials/whisper_e2e.py: the exported beam-search
Whisper model is wrapped with the audio-decode + log-mel pre-processor and the
BPE post-processor, producing a single graph with an `audio_stream` input and a
`str` output. Fails loudly; there is no partial-success path.
"""

import os
import sys

import numpy as np
import onnx
import onnxruntime as ort
from onnx import TensorProto
from onnxruntime.quantization import QuantType, quantize_dynamic
from onnxruntime_extensions import PyOrtFunction, get_library_path, util
from onnxruntime_extensions.cvt import gen_processing_models
from transformers import WhisperProcessor, WhisperForConditionalGeneration


def main() -> int:
    model_name = sys.argv[1]
    output_path = sys.argv[2]
    work_dir = sys.argv[3]

    os.makedirs(os.path.dirname(output_path), exist_ok=True)
    os.makedirs(work_dir, exist_ok=True)

    print(f"[export] loading {model_name}", flush=True)
    processor = WhisperProcessor.from_pretrained(model_name)
    model = WhisperForConditionalGeneration.from_pretrained(model_name)

    # Beam-search core export via the onnxruntime whisper converter.
    from onnxruntime.transformers.models.whisper.convert_to_onnx import main as convert_main

    core_dir = os.path.join(work_dir, "core")
    os.makedirs(core_dir, exist_ok=True)
    convert_argv = [
        "--model_name_or_path", model_name,
        "--cache_dir", os.path.join(work_dir, "cache"),
        "--output", core_dir,
        "--precision", "fp32",
        "--optimize_onnx",
        "--use_external_data_format",
        "--overwrite",
        "--use_forced_decoder_ids",
    ]
    print(f"[export] convert_to_onnx {' '.join(convert_argv)}", flush=True)
    convert_main(convert_argv)
    model_slug = model_name.rsplit("/", 1)[-1]
    core_model_path = os.path.join(core_dir, f"{model_slug}_beamsearch.onnx")
    if not os.path.exists(core_model_path):
        raise RuntimeError(f"whisper converter produced no model at {core_model_path!r}")

    print("[export] generating pre/post processing graphs", flush=True)
    pre_model, post_model = gen_processing_models(
        processor,
        pre_kwargs={"USE_AUDIO_DECODER": True, "USE_ONNX_STFT": True},
        post_kwargs={},
    )

    core = onnx.load(core_model_path)
    fused = util.quick_merge(pre_model, core, post_model)

    fp32_path = os.path.join(work_dir, "whisper-e2e-fp32.onnx")
    onnx.save(
        fused,
        fp32_path,
        save_as_external_data=False,
        all_tensors_to_one_file=True,
    )
    print("[export] quantizing assembled E2E graph", flush=True)
    quantize_dynamic(
        fp32_path,
        output_path,
        weight_type=QuantType.QInt8,
        op_types_to_quantize=["MatMul", "Gemm", "Gather"],
        use_external_data_format=False,
        extra_options={
            "EnableSubgraph": True,
            "MatMulConstBOnly": True,
            "DefaultTensorType": TensorProto.FLOAT,
        },
    )

    # Prove the produced graph honours the contract the Rust runtime relies on
    # before it is ever pinned.
    produced = onnx.load(output_path)
    input_names = {i.name for i in produced.graph.input}
    output_names = {o.name for o in produced.graph.output}
    required_inputs = {
        "audio_stream", "max_length", "min_length", "num_beams",
        "num_return_sequences", "length_penalty", "repetition_penalty",
        "decoder_input_ids",
    }
    missing = required_inputs - input_names
    if missing:
        raise RuntimeError(
            f"produced graph is missing required inputs {sorted(missing)}; "
            f"it has {sorted(input_names)}"
        )
    if "str" not in output_names:
        raise RuntimeError(
            f"produced graph has no `str` output; it has {sorted(output_names)}"
        )

    # Metadata alone cannot prove that contrib/custom-domain nodes survived
    # merging and quantization. Load the graph through the same ORT Extensions
    # registration contract used by the runtime before any bytes are pinned.
    session_options = ort.SessionOptions()
    session_options.register_custom_ops_library(get_library_path())
    ort.InferenceSession(
        output_path,
        sess_options=session_options,
        providers=["CPUExecutionProvider"],
    )

    print(f"[export] wrote {output_path} ({os.path.getsize(output_path)} bytes)", flush=True)
    print(f"[export] inputs: {sorted(input_names)}", flush=True)
    print(f"[export] outputs: {sorted(output_names)}", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
'@
Set-Content -LiteralPath $exportScript -Value $exportSource -Encoding UTF8

Info "Exporting end-to-end graph (this takes several minutes)"
& $venvPython $exportScript $Model $OutputPath (Join-Path $WorkRoot 'scratch')
if ($LASTEXITCODE -ne 0) {
    Die "SYNAPSE_WHISPER_BUILD_EXPORT_FAILED exit_code=$LASTEXITCODE model=$Model output=$OutputPath remediation=inspect the export output above; the artifact was NOT produced and no pin was changed"
}
if (-not (Test-Path -LiteralPath $OutputPath -PathType Leaf)) {
    Die "SYNAPSE_WHISPER_BUILD_ARTIFACT_MISSING path=$OutputPath remediation=the exporter reported success but wrote no file; treat this as an export failure"
}

$artifact = Get-Item -LiteralPath $OutputPath
$length = [int64]$artifact.Length
$sha = (Get-FileHash -LiteralPath $OutputPath -Algorithm SHA256).Hash.ToUpperInvariant()
Info "Produced artifact path=$OutputPath length=$length sha256=$sha"

if ($NoRepin) {
    Info "-NoRepin: committed pins left unchanged. To adopt these bytes, set sha256=$sha length=$length in $pinPath and WHISPER_TINY_INT8_ONNX_SHA256 in $registryPath."
    exit 0
}

$pin = Get-Content -LiteralPath $pinPath -Raw -Encoding UTF8 | ConvertFrom-Json
$previousSha = [string]$pin.sha256
$pin.sha256 = $sha
$pin.length = $length
$pin.provenance.pinned_by = "produced by scripts/build-whisper-e2e-onnx.ps1 from $Model on $((Get-Date).ToUniversalTime().ToString('o'))"
$pin | ConvertTo-Json -Depth 32 | Set-Content -LiteralPath $pinPath -Encoding UTF8
Info "Rewrote installer pin $pinPath ($previousSha -> $sha)"

$registryText = Get-Content -LiteralPath $registryPath -Raw -Encoding UTF8
$pattern = '(WHISPER_TINY_INT8_ONNX_SHA256\s*:\s*&str\s*=\s*)"sha256:[0-9a-fA-F]{64}"'
if (-not [regex]::IsMatch($registryText, $pattern)) {
    Die "SYNAPSE_WHISPER_BUILD_REPIN_CONSTANT_NOT_FOUND path=$registryPath remediation=the daemon constant could not be located; the installer pin was updated but the daemon constant was not — reconcile by hand before building"
}
$registryText = [regex]::Replace($registryText, $pattern, ('${1}"sha256:' + $sha.ToLowerInvariant() + '"'))
Set-Content -LiteralPath $registryPath -Value $registryText -Encoding UTF8 -NoNewline
Info "Rewrote daemon constant WHISPER_TINY_INT8_ONNX_SHA256 in $registryPath"

Write-Host ''
Info 'Next steps:'
Info "  1. Review and commit the pin changes ($pinPath, $registryPath)."
Info "  2. `$env:SYNAPSE_WHISPER_ONNX_SOURCE = '$OutputPath'"
Info "  3. scripts\synapse-setup.ps1 -SourceDir $resolvedSource"
