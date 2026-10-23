Fixtures de `disk_streamer`: 1 s, mono, 44.1 kHz, seno de 440 Hz (amplitud ~0.0625).

    ffmpeg -f lavfi -i "sine=frequency=440:sample_rate=44100:duration=1" -af volume=0.5 -ac 1 -c:a pcm_s16le tone.wav
    flac --best tone.wav -o tone.flac
    lame -b 64 tone.wav tone.mp3
