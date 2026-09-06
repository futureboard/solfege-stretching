# Validation & Implementation Roadmap

[สารบัญ](README.md) · **Proposed tests/targets — ยังไม่มีผล benchmark**

## 1. หลักการเปรียบเทียบ

ต้องแยก correctness, perceptual quality และ runtime cost ไม่ให้คะแนนเสียงดีมาปกปิด length/clock bug และไม่ถือว่า sample-accurate endpoint แปลว่า transient ทุกจุดคมชัด

Baseline อย่างน้อย: original, Tape, naive OLA, WSOLA, PV และ Solfege candidate; Rubber Band/SoundTouch ใช้เป็น external references ได้เมื่อ setup พร้อม Commercial DAWs เป็น optional manual renders ที่ต้องบันทึก edition/version/mode/settings ไม่รวมผลที่คนละ ratio หรือคนละ source

## 2. Corpus

| หมวด | ตัวอย่าง | สิ่งที่จับได้ |
|---|---|---|
| Analytical | silence, impulse train, sine 55/110/440 Hz, chirp, harmonics | gain, pitch, alias, map/length |
| Drums | kick/snare, dense hats, cymbal tails | double attacks, smear, looping tail |
| Tuned percussion | 808 glide, marimba, piano attacks | pitch-transient trade-off |
| Voice | Thai/English speech, dry singing, vibrato, breath/sibilants | F0 errors, consonants, formants |
| Mono instrument | bass, bowed strings, legato wind | flutter, low-frequency stability |
| Polyphonic | piano chords, strummed guitar, choir | phase coherence, partial interference |
| Mix | acoustic ensemble, electronic full mix, live reverb | hybrid leakage, image, wash |
| Stereo/group | identical/inverted channels, fixed delay, real drum mics | independent channel decisions |
| Edges | empty, 1 frame, shorter than window, clipped source, NaN input | safety, padding, state machine |

ใช้เสียงสังเคราะห์เองและเสียงที่มีสิทธิ์ใช้งาน; บันทึก source hash, rate, channels และ category เก็บ corpus ที่ใช้ tuning แยกจาก held-out set ไม่เลือกเฉพาะเสียงที่ algorithm ชนะ

## 3. Parameter matrix

- `alpha`: 0.5, 0.75, 0.9, 1, 1.1, 1.5, 2; stress 0.25/4 เป็นรายงานนอก quality target
- Pitch: −12, −7, −1, 0, +1, +7, +12 semitones; ทดสอบ combined stretch/pitch และ internal ratio ด้วย
- Map: constant, local compression/expansion, anchor ใกล้กัน, invalid order, tempo ramps, long-duration drift
- Block sizes: 1, 17, 64, 127, 256, 1024 และ random chunks ที่มี seed
- Rates: 44.1/48/96 kHz; mono/stereo และ group เมื่อ milestone รองรับ
- Operations: start, short EOF, drain, seek, loop, cancel, plan replacement, underrun

ไม่ต้องคูณทุกมิติเป็น full Cartesian product ตั้งแต่แรก: correctness ใช้ small fixtures หลายรูปแบบ, quality ใช้ representative combinations, stress และ long-run แยก suite

## 4. Correctness gates

| Gate | เกณฑ์เสนอ |
|---|---|
| Identity | float bypass sample-exact เมื่อไม่มี edits; gain/pitch/formant identity |
| Endpoint | offline output เท่ากับ `M` frames ทุกครั้ง รวม empty case |
| Map/inverse | monotonic; inverse round-trip error ≤ 0.5 frame ในโดเมนทดสอบ |
| Hard anchors | logical scheduler error ≤ 1 sample frame; ตรวจ attack envelope แยก |
| Steady sine pitch | median error ≤ 1 cent หลังตัด startup/tail ในช่วง supported |
| Numerical | ไม่มี NaN/Inf จาก finite input; bounds respected; silence ไม่มี energy ที่สร้างเอง |
| Block invariance | plan เดียว/seed เดียวให้ผลตรงภายใน float tolerance ที่ประกาศ; เป้าหมาย RMS difference ≤ −100 dBFS |
| Drain | consume input ครบ, ไม่มี frame ซ้ำ, tail จบ, no infinite loop |
| Invalid edit | typed error ก่อนเริ่ม render; ไม่แก้ anchor เงียบ ๆ |
| Seek | logical output start ถูก; เปรียบเทียบกับ full render หลัง warm-up tolerance |

แยก `anchor sample accuracy` ของ map จาก `onset measurement`: ใช้ impulse fixture กับ slice engine คาดตำแหน่ง sample ได้ แต่ PV อาจกระจายพลังงานแม้ timestamp ถูก ต้องวัด attack width และ onset error อีกชุด

## 5. คุณภาพเสียง

### Objective diagnostics

- Pitch error ใช้ cents จาก known synthetic F0; สำหรับเสียงจริงใช้ independent estimator พร้อม confidence และตรวจด้วยหู ไม่ใช้ detector ตัวเดียวกับ engine เป็นกรรมการตัวเดียว
- Transient วัด onset shift, rise-time width, peak envelope และพลังงานก่อน attack ในหน้าต่างที่ระบุ
- Spectral distortion ใช้ aligned local spectra เทียบกับ reference ที่สร้างทาง analytic ได้; ไม่เอา raw waveform SNR ระหว่างคนละเวลาเป็น quality score
- Stereo: identical channels ต้องยัง identical; inverted channels ต้องรักษาความสัมพันธ์; delayed fixture วัด phase/delay error ตาม mode พร้อมดู mono-sum spectrum
- Loudness/peak: วัดก่อน/หลังโดยไม่ auto-normalize render; ทำ loudness match เฉพาะสำเนาที่ใช้ฟัง
- Formant: ใช้ source-filter synthetic vowel ที่รู้ resonances และร้องจริง ตรวจ envelope ไม่ถือว่า pitch accuracy พิสูจน์ formant preservation

เกณฑ์ onset error ≤ 1 ms บน sparse percussion เป็น **เป้าหมายแรก**; กำหนด confidence intervals และแยก tuned percussion/dense hats ก่อนใช้เป็น release gate ทั้ง corpus

### Listening protocol

1. สุ่มชื่อ output ปิด engine labels ใช้ gain match เดียวกันและเปิด replay ได้
2. ฟัง original เพื่อรู้เนื้อเสียง และเทียบ transformed outputs ที่ target timing/pitch เดียวกัน
3. ให้คะแนนแยก attack, tonal stability, vocal naturalness, stereo image และ overall preference
4. ใช้ reference ที่สร้างได้จริง/hidden reference สำหรับกรณีที่มี ไม่อ้างว่ามี perfect stretched ground truth ของเพลงทุกเพลง
5. เริ่ม pilot อย่างน้อย 5 listeners เพื่อหา failure; ถ้าจะกล่าวอ้างเหนือกว่าคู่แข่งต้องเพิ่มจำนวน/ออกแบบสถิติ รายงาน paired results และ uncertainty
6. รายงานแพ้/เสมอ/ชนะตามหมวดและ ratio ไม่สรุปคะแนนเดียวปกปิดกรณีเสียหาย

ABX ใช้ทดสอบความแตกต่างได้ แต่ไม่ได้บอกว่าเสียงไหนดีกว่า จึงไม่ใช้แทน preference/quality ratings

## 6. Runtime gates

บันทึก CPU model, OS, compiler, build flags, sample rate, block size, engine/version, thread count และ warm/cold state ทุกครั้ง

| Metric | เกณฑ์/วิธี |
|---|---|
| Callback allocation | 0 allocations และ 0 deallocations หลัง prepare |
| Callback blocking | ไม่มี file IO/mutex waits/FFT planning |
| Callback budget | target p99 ≤ 50% ของ block deadline บน reference machine ที่เลือก; รายงาน max และ underruns ด้วย |
| ตัวอย่าง deadline | 256/48000 = 5.333 ms; p99 target ≈ 2.667 ms ต่อ test configuration |
| Offline throughput | รายงาน processed audio seconds/wall second พร้อมแยก analysis/render |
| Memory | bounded by config; run ยาวแล้วไม่โตตามเวลา ยกเว้น cache ที่อยู่ worker |
| Buffer stress | ratio automation/seek/EOF ไม่ overflow และไม่ busy-spin |

Reference machine ยังไม่ได้เลือก จึงยังไม่เป็น performance promise การทดสอบ 1 engine ไม่ถือว่าผ่านหลาย tracks และ average CPU ต่ำไม่ได้พิสูจน์ไม่มี callback spikes

## 7. Milestones และเกณฑ์จบ

| ขั้น | ส่งมอบ | ผ่านเมื่อ |
|---|---|---|
| M0 — Scaffold/contracts | ตรวจ parent workspace; library/CLI, units, fixtures, map validation | build ได้และ mapping/property tests ผ่าน |
| M1 — Bypass/Tape | WAV offline IO, anti-aliased resampler, exact length | identity, pitch-rate relation, alias diagnostics, EOF ผ่าน |
| M2 — WSOLA | shared-channel search, constant stretch, transpose composition | supported mono/stereo matrix ผ่าน; มี listening report |
| M3 — Warp/Percussive | anchors, slicing/tail policy, protected attack scheduler | hard anchors, constraints, phase-linked fixture ผ่าน |
| M4 — Polyphonic | STFT reconstruction, PV, phase locking/transient handling | tonal/percussion diagnostics และ A/B กับ M2 |
| M5 — File playback | bounded buffers, seek/loop, worker cache, control timestamps | allocation/budget/drain/seek tests ผ่าน |
| M6 — Vocal editor core | F0 confidence, note edits, formant policy, voiced/unvoiced | ร้อง/speech held-out tests; แก้ detection ได้ |
| M7 — Hybrid/group | HPSS branches, multitrack coherence, optional multi-resolution | แสดงประโยชน์เหนือ single-engine baseline ตามหมวด |

แต่ละขั้นถือว่าจบเมื่อมี implementation + evidence ไม่ใช่มีชื่อ mode ใน enum เวลาในการพัฒนาให้ประมาณหลัง M1/M2 มี profiling จริง

## 8. Report schema ที่ใช้ซ้ำ

```json
{
  "status": "not_run",
  "engine": "solfege-wsola",
  "engine_revision": null,
  "fixture_hash": null,
  "sample_rate": 48000,
  "channels": 2,
  "alpha": 1.5,
  "pitch_semitones": 0,
  "expected_frames": null,
  "actual_frames": null,
  "pitch_error_cents": null,
  "onset_error_ms": null,
  "callback_p99_ms": null,
  "underruns": null,
  "listening_notes": null
}
```

ค่า `null` หมายถึงยังไม่วัด ไม่ใช่ศูนย์ และตัวอย่างนี้ไม่ใช่ผลทดลองจริง

## 9. ความเสี่ยงที่ต้องปิดก่อน release

| ความเสี่ยง | การตอบสนอง |
|---|---|
| transient protection ทำให้ map infeasible | typed conflict + explicit edit choice |
| pitch automation ทำให้ anchor drift | test intermediate/output clock composition |
| channel decisions แยกกัน | shared decision state + phase fixtures |
| F0 ผิดในเสียงร้องมี reverb | confidence/manual correction และไม่บังคับ mono synthesis |
| Hybrid ทำให้เสียงบาง/attack ซ้ำ | mask leakage diagnostics และ aligned branches |
| ratio extremes ใช้ CPU/หน่วยความจำเกิน | validate capability ก่อน prepare และ bounded runtime |
| แหล่งเสียง/ไลบรารีเทียบไม่พร้อม | ใช้ owned synthetic baselines ก่อน; ไม่สร้างคะแนน commercial ขึ้นเอง |

**อัปเดต 2026-09-06:** correctness gates ในหน้านี้ถูก implement และรันจริงแล้ว (`solfege selftest`, `cargo test` 36 ตัว) และ runtime gates วัดแล้วด้วย `solfege bench` ผลอยู่ใน [Implementation §4](implementation.md) — ทุก gate ที่รันได้โดยไม่ต้องใช้ผู้ฟังผ่านที่ 44.1/48/96 kHz

M5 (real-time playback) implement แล้วเช่นกัน: engine ทำงานใน audio callback จริง เล่นจนจบครบทุกโหมดโดย **0 underruns** และเปลี่ยน plan ระหว่างเล่นได้ (ดู [Implementation §3.1](implementation.md))

สิ่งที่ **ยังไม่ได้ทำ**: listening protocol (§5) ทั้งหมด, การเทียบกับ external references (Rubber Band/SoundTouch/DAW), การเลือก reference machine, M6–M7 ตัวเลขคุณภาพเสียงจึงยังไม่มี และไม่มีคำกล่าวอ้างเรื่องคุณภาพในงานรอบนี้
