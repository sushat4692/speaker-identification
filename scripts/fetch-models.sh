#!/usr/bin/env bash
#
# Fetch the ONNX models the diarization pipeline needs.
#
# HuggingFace's `pyannote/segmentation-3.0` is gated (requires accepting
# usage terms). sherpa-onnx re-hosts the same model, ungated, on its own
# GitHub releases — that's what this script pulls from. See the plan doc's
# "モデル取得" section.
#
# NOTE the upstream typo in the embedding-models release tag
# ("speaker-recongition-models") — that is not a mistake in this script,
# do not "fix" it.

set -euo pipefail

MODELS_DIR="${1:-models}"
mkdir -p "$MODELS_DIR"
cd "$MODELS_DIR"

fetch_and_extract() {
  local url="$1"
  local archive
  archive="$(basename "$url")"

  if [[ -f "$archive" ]]; then
    echo "already downloaded: $archive"
  else
    echo "downloading: $url"
    curl -L --fail -o "$archive" "$url"
  fi

  case "$archive" in
    *.tar.bz2) tar xjf "$archive" ;;
    *.tar.gz)  tar xzf "$archive" ;;
    *)
      echo "don't know how to extract $archive" >&2
      exit 1
      ;;
  esac
}

echo "=== Speaker segmentation model (pyannote 3.0, sherpa-onnx mirror) ==="
fetch_and_extract "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-segmentation-models/sherpa-onnx-pyannote-segmentation-3-0.tar.bz2"

echo
echo "=== Speaker embedding model (3D-Speaker CAM++, zh_en 16k common-advanced) ==="
echo "Per the plan doc, which embedding model performs best on Japanese is"
echo "UNVERIFIED and is Phase 1's main tuning question. This is a starting"
echo "candidate, not a settled choice -- fetch alternatives below to A/B."
curl -L --fail -O "https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/3dspeaker_speech_campplus_sv_zh_en_16k-common_advanced.onnx"

cat <<'EOF'

Done. Optional embedding-model alternatives to compare during Phase 1
tuning (same "speaker-recongition-models" release tag; fetch by hand as
needed):

  wespeaker_en_voxceleb_resnet34_LM.onnx   (WeSpeaker, English VoxCeleb)
  nemo_en_titanet_large.onnx               (NeMo TitaNet)
  3dspeaker_speech_eres2net_*.onnx          (3D-Speaker, alternative arch)

See:
  https://github.com/k2-fsa/sherpa-onnx/releases/tag/speaker-recongition-models
EOF
