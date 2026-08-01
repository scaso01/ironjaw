#!/bin/bash
# Setup Chatterbox-Turbo TTS server on workstation
# Requires: Python 3.11+, pip, CUDA/Vulkan GPU

set -e

INSTALL_DIR="$HOME/Tools/chatterbox"
PORT=8090

echo "Installing Chatterbox-Turbo TTS server..."

mkdir -p "$INSTALL_DIR"
cd "$INSTALL_DIR"

# Create venv
python -m venv .venv
source .venv/bin/activate

# Install chatterbox + server dependencies
pip install chatterbox-tts aiohttp

# Create server script
cat > serve.py << 'PYEOF'
"""Chatterbox-Turbo TTS HTTP API server.

POST /v1/audio/speech
  {"model": "chatterbox", "input": "text", "voice": "reference.wav"}
  Returns: audio/wav

GET /v1/voices
  Returns: list of available voice IDs

GET /health
  Returns: {"status": "ok"}
"""
import io
import json
import os
import sys
from pathlib import Path

from aiohttp import web
import torch
import torchaudio

# Lazy-load model
_model = None
VOICES_DIR = Path(__file__).parent / "voices"


def get_model():
    global _model
    if _model is None:
        from chatterbox.tts import ChatterboxTTS
        device = "cuda" if torch.cuda.is_available() else "cpu"
        print(f"Loading Chatterbox model on {device}...")
        _model = ChatterboxTTS.from_pretrained(device=device)
        print("Model loaded.")
    return _model


async def handle_health(request):
    return web.json_response({"status": "ok"})


async def handle_voices(request):
    voices = []
    if VOICES_DIR.exists():
        for f in sorted(VOICES_DIR.glob("*.wav")):
            voices.append({
                "id": f.stem,
                "name": f.stem.replace("-", " ").replace("_", " ").title(),
            })
    return web.json_response(voices)


async def handle_speech(request):
    data = await request.json()
    text = data.get("input", "")
    voice_id = data.get("voice", "default")

    if not text:
        return web.json_response({"error": "input required"}, status=400)

    model = get_model()

    # Find reference audio for voice cloning
    ref_path = VOICES_DIR / f"{voice_id}.wav"
    if ref_path.exists():
        wav = model.generate(text, audio_prompt=ref_path)
    else:
        wav = model.generate(text)

    # Convert to WAV bytes
    buf = io.BytesIO()
    torchaudio.save(buf, wav, model.sr, format="wav")
    buf.seek(0)

    return web.Response(body=buf.read(), content_type="audio/wav")


app = web.Application()
app.router.add_get("/health", handle_health)
app.router.add_get("/v1/voices", handle_voices)
app.router.add_post("/v1/audio/speech", handle_speech)

if __name__ == "__main__":
    port = int(os.environ.get("PORT", "8090"))
    print(f"Chatterbox TTS server starting on port {port}")
    web.run_app(app, host="0.0.0.0", port=port)
PYEOF

# Create voices directory for reference WAV files
mkdir -p voices

echo ""
echo "Setup complete!"
echo ""
echo "Place reference WAV files (10-15s each) in: $INSTALL_DIR/voices/"
echo "  Example: voices/kitt.wav, voices/drclaw.wav, etc."
echo ""
echo "Start server:"
echo "  cd $INSTALL_DIR && .venv/bin/python serve.py"
echo ""
echo "Test:"
echo "  curl http://localhost:8090/health"
echo "  curl http://localhost:8090/v1/voices"
echo "  curl -X POST http://localhost:8090/v1/audio/speech \\"
echo "    -H 'Content-Type: application/json' \\"
echo "    -d '{\"input\": \"Hello world\", \"voice\": \"default\"}' \\"
echo "    --output test.wav"
