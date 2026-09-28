# Implementation: สิ่งที่สร้างแล้วและวิธีใช้

สถานะ **Elastic rework** · 2026-09-28 · [สารบัญ](README.md) · [Summary](summary.md) · [DSP](dsp.md) · [System Design](system-design.md) · [Validation](validation.md)

เอกสารนี้บอกว่าอะไร **มีจริงในโค้ดแล้ว** วัดได้เท่าไร และอะไร **ยังไม่มี** ตัวเลขทั้งหมดเป็นผลรันจริงบนเครื่องพัฒนา (ไม่ใช่ reference machine ที่ตกลงกัน เพราะยังไม่ได้เลือก)

## 0. รื้ออะไร และทำไม

รอบก่อนมี engine 6 ตัว (Polyphonic PV + low-band path, Hybrid HPSS, Monophonic WSOLA, Percussive slicing, Tape, Texture) ที่ถูกแก้เป็นชั้น ๆ ตามอาการที่ได้ยิน — เบสแตก, วูบเหมือน sidechain, กระตุกตอนขยับ slider — แต่ละรอบแก้อาการหนึ่งแล้วแลกกับอีกอาการ (ดูประวัติใน git ก่อน `754c66f`) และการขยับ control ระหว่างเล่นทุกครั้ง = **สร้าง engine ใหม่ แล้ว crossfade สอง render ที่ phase ไม่สัมพันธ์กัน** ซึ่งได้ยินเป็นเสียงวูบทุกครั้งที่ลาก slider

รอบนี้ **เปลี่ยน engine ทั้งชุด** แต่ **คง architecture เดิม** ไว้ทั้งหมด: time map, edit document, plan compiler, process contract (`prepare/reset/process`), offline render, stream + prefetch worker, WAV IO, analysis — contract เดิมทุกข้อยังบังคับใช้และมี test

| เดิม | ตอนนี้ |
|---|---|
| Polyphonic (PV + identity locking + low-band path + re-anchor) | **Elastic Pro** — phase-gradient heap integration |
| Hybrid (HPSS) | รวมเข้า Elastic Pro (document เก่าที่เลือก hybrid เปิดเป็น Elastic Pro) |
| — | **Elastic Efficient** — kernel เดียวกัน หน้าต่างสั้นลงครึ่ง overlap 4x |
| Percussive (slicing) | **Rhythmic** — kernel เดียวกัน หน้าต่าง 21 ms ล็อก attack เต็มหน้าต่าง |
| Monophonic (WSOLA) | **Soloist** — pitch-synchronous overlap-add (TD-PSOLA) |
| Tape | **Varispeed** (engine เดิม) |
| Texture | Texture (engine เดิม) |
| slider = สร้าง engine ใหม่ + crossfade | slider = **retarget engine ที่กำลังเล่นอยู่** ไม่มี crossfade |

document JSON เก่ายังเปิดได้: serde alias แปลง `polyphonic`/`hybrid` → `elastic_pro`, `percussive` → `rhythmic`, `monophonic` → `soloist`, `tape` → `varispeed` (มี test)

## 1. รันได้ทันที

```bash
cargo run --release --bin solfege -- selftest            # correctness gates
cargo run --release --bin solfege -- quality             # bass/vowel/drum/tail/stereo ทุกโหมด
cargo run --release --bin solfege -- quality --alpha 1.5 --semitones -5 --mode elastic-pro
cargo run --release --bin solfege -- stability --semitones 3
cargo run --release --bin solfege -- protect             # transient handling เปิด/ปิด ระดับเปลี่ยนไหม
cargo run --release --bin solfege -- bench --block 256
cargo run --release --bin solfege -- render --in in.wav --out out.wav \
                                    --alpha 1.25 --semitones -2 --mode elastic-pro --formant preserve
cargo run --release --features playback --bin solfege -- play --in in.wav --sweep 0.5
cargo test --release                                     # 48 tests
cargo run --release --features demo --bin solfege-demo   # GUI
```

ชื่อโหมดใน CLI: `auto`, `elastic-pro` (หรือ `elastic`/`pro`), `elastic-efficient`, `rhythmic`, `soloist`, `varispeed`, `texture`, `bypass` — ชื่อเก่า (`polyphonic`, `monophonic`, `percussive`, `tape`, …) ยังใช้ได้

## 2. โหมด

ทุกผลิตภัณฑ์ที่ศึกษาใน [research.md](research.md) มาถึงชุดเดียวกัน: โหมดทั่วไปแบบสเปกตรัมหนึ่งตัว, รุ่นประหยัด, โหมดที่ attack มาก่อน, โหมดเสียงเดี่ยว, varispeed และ FX (Pro Tools Elastic Audio: Polyphonic/Rhythmic/Monophonic/Varispeed/X-Form; zplane élastique: Pro/Efficient/Soloist; Logic Flex: Polyphonic/Slicing/Rhythmic/Monophonic/Speed/Tempophone; Ableton: Complex Pro/Complex/Beats/Tones/Re-Pitch/Texture) ชุดของเราตามโครงนั้น **แต่ implementation เป็นของเราเองทั้งหมด** ไม่มีการอ้างว่าเหมือนหรือเทียบเท่าผลิตภัณฑ์ใด

| โหมด | ทำอะไร | หน้าต่าง @48k | เหมาะกับ |
|---|---|---|---|
| **Elastic Pro** | PGHI phase vocoder, overlap 8x, ล็อก attack | 4096 (85 ms) | ทุกอย่าง: full mix, คีย์บอร์ด, pad, ร้องมีดนตรี |
| **Elastic Efficient** | kernel เดียวกัน overlap 4x | 2048 (43 ms) | เหมือน Pro ด้วยงานราว 1/4 |
| **Rhythmic** | kernel เดียวกัน, ล็อก attack ทั้งหน้าต่าง, ยอมให้อัตราอ่านเบี่ยงได้มากกว่า | 1024 (21 ms) | กลอง, loop, rhythm part |
| **Soloist** | TD-PSOLA: grain สองคาบวางใหม่ตาม pitch | ตามคาบเสียง | ร้องเดี่ยว, เบส, lead — formant อยู่กับที่เองโดยธรรมชาติ |
| **Varispeed** | band-limited resampling ตาม `W⁻¹` | – | pitch ตามความเร็วแบบเทป |
| **Texture** | granular มี seed | – | FX (ไม่ใช่ fallback) |
| **Auto** | เลือกจาก analysis: percussive → Rhythmic, monophonic → Soloist, อื่น ๆ → Elastic Pro; ไม่มี analysis → Elastic Pro | | |

ทุกโหมด (ยกเว้น Varispeed/Bypass) ย้าย pitch ได้อิสระ ±24 st และคุม formant ได้ (follow / preserve / shift)

## 3. Elastic: engine เดียว สามพรีเซ็ต

```text
source --(scheduler: map + transient locks)--> kernel --> z --(resampler ที่อัตรา p)--> output
```

`src/engines/elastic/{spectral,schedule,mod}.rs`

### 3.1 Kernel: phase-gradient heap integration

อิงจาก Průša & Holighaus, *Phase Vocoder Done Right* (EUSIPCO 2017) — RTPGHI สำหรับ PV — เขียนในรูป **rotation**: bin ขาออก = bin ขาเข้า × `e^{jθ}` และ `θ` เป็นตัวแปรเดียวที่ต้องหา

1. **วิเคราะห์ด้วย FFT เดียวต่อแชนเนล** — FFT ของเฟรมดิบ แล้วได้ทั้ง spectrum ที่คูณ Hann และ spectrum ที่คูณ *อนุพันธ์* ของ Hann ด้วย convolution 3 tap (ทั้งคู่มี spectrum 3 tap) → ความถี่ reassigned ของทุก bin ในรูปปิด `ω = ω_k − Im(X_dh·conj X_h)/|X_h|²` ใช้เป็นแกน unwrap phase advance ทำให้ hop ยาว (บีบเวลามาก) ไม่ unwrap ผิด partial · zero-phase framing
2. **Heap** — จัดลำดับ bin จากดังไปเบา แต่ละ bin รับ rotation จาก (ก) เฟรมก่อนของตัวเอง: phase advance ที่วัดได้ ปรับจาก analysis hop เป็น synthesis hop หรือ (ข) bin ข้างเคียงในเฟรมเดียวกันที่ตั้งค่าแล้ว: **copy rotation** ซึ่งรักษาความต่าง phase ระหว่าง bin ของ input = local group delay = รูปทรงของเสียง broadband · ที่ ratio 1 rotation เป็นศูนย์พอดี → engine เต็มตัวที่ ratio 1 ออกมาเท่า input (−70 dBFS ขึ้นไป, มี test)
3. **Attack** — เฟรมที่มี onset อยู่ในหน้าต่าง: bin ที่โตเกิน 6 dB ถูก seed จาก phase ที่วิเคราะห์ได้ ไม่รับ rotation จากเสียงที่ดังค้างอยู่ก่อน (การรับ rotation เดียวกันทั้ง click คือการบิดแบบ Hilbert: peak ของ impulse ตกเหลือ 0.707 และมีหางสองข้าง — วัดแล้วจริง)
4. **Stereo** — rotation เดียวต่อ bin ใช้ทุกแชนเนล คำนวณจาก cross-power รวมทุกแชนเนล (ไม่แพ้ phase inversion) · identical ยังเหมือนบิตต่อบิต, inverted ยัง −1.000, mic delay 24 → 24 frames
5. **Formant** — "true envelope" (Röbel & Rodet 2005): iterate `A = max(A, lifter(A))` ให้ envelope ขี่ยอด harmonic แทนที่จะเฉลี่ย · lifter ปรับตาม pitch ของเฟรม (หา rahmonic แรกใน cepstrum แล้วตัดที่ ¾ ของคาบ, สูงสุด 3 ms) · ปรับ gain ให้พลังงานเฟรมคงเดิม เปลี่ยน formant ต้องไม่เปลี่ยนความดัง

### 3.2 Scheduler: transient lock

PV ทำ attack เบลอเพราะเฟรมที่มี attack ถูก *อ่าน* ห่าง `Ha` แต่ *เขียน* ห่าง `Hs` — ทุกเฟรมวางสำเนาของ hit ไว้คนละเวลา สำเนาจะซ้อนตรงกันก็ต่อเมื่อเฟรมรอบ ๆ hit อ่านด้วยอัตราเดียวกับที่เขียน **(unity)**

- onset detector แบบ causal (พลังงานของ first difference ต่อ block 2.7 ms เทียบค่าเฉลี่ย 8 block ก่อนหน้า, refractory 50 ms) วิ่งบน input จริง **ไม่ต้องมี analysis ล่วงหน้า** offline และ live จึงทำตัวเหมือนกัน
- รอบแต่ละ onset `o` ตำแหน่งอ่านเดินตามเส้น `a(t) = o + q·(t − W(o))` ตลอดหน้าต่าง (q = unity ของ kernel) → hit ลง **ตรง `W(o)`** ตามที่ map กำหนด และผ่าน kernel เหมือนไม่มีการยืดเลย
- เวลาที่ยืมไปคืนด้วย ramp สองข้างที่อัตราอ่านเบี่ยงจาก map ไม่เกิน ×1.6 (Rhythmic ×2.5), ramp ยาวไม่เกิน 200 ms
- lock จะถูกรับก็ต่อเมื่อ **ทั้ง span อยู่ใน segment เดียวของ map** → anchor ทุกตัวและ endpoint ยังตรงเป๊ะ · onset ที่ถี่เกินจนซ้อนกันจะเสีย lock ตัวหลัง (ไม่ใช่เสีย anchor)
- ภายใน lock เดียวกัน เฟรมถัดไปอ่านห่าง **หนึ่ง hop พอดี** ไม่ปัดเศษใหม่ — hit ที่ `W(o)` ตกครึ่ง sample เคยปัดสลับไปมาทำให้ hop แกว่ง ±1 และ attack เบลอเหลือ −48 dB (เจอจาก test แล้วแก้)
- ทุกอย่างเป็นฟังก์ชันของ map + onset → ผลไม่ขึ้นกับ block size (block invariance ยังเป็นบิตเดียวกัน)

### 3.3 Pitch: stretch แล้ว resample บน clock เดียว

kernel ยืด `α·p` ลง timeline ภายใน `z` แล้ว sinc resampler (16 zero-crossings, cutoff ตาม p) อ่าน `z` กลับที่อัตรา `p` — ทั้งสองอยู่ใน engine เดียว บน clock เดียว `u(t)` (ตำแหน่ง z ของ output frame t) · เปลี่ยน `p` = เปลี่ยนความชันของ `u` ตั้งแต่ block ถัดไป (glide 35 ms บน grid 64 frame ที่ตายตัว) scheduler ถามเวลาของเฟรม kernel ผ่าน `u` ตัวเดียวกัน attack จึงผ่าน kernel ที่ unity เสมอไม่ว่าจะย้าย pitch เท่าไร แล้ว resampler ย้ายทั้งเวลาและ pitch พร้อมกันแบบแม่นยำ

### 3.4 Retarget: เปลี่ยน map/pitch/formant โดยไม่สร้าง engine ใหม่

`StretchEngine::retarget(&mut map, pitch, formant)` — engine เก็บ phase state, timeline `z` และตำแหน่ง source ไว้ แล้ว relabel output frame เข้าพิกัดของ map ใหม่ · ตำแหน่งอ่านไม่กระโดด (ส่วนต่างที่เกิดจาก lock ของ map เก่า/ใหม่ค่อย ๆ จางไป) · **ไม่ allocate ไม่ free** — map เก่าถูกสลับคืนใส่กล่องให้ worker ทำลาย

## 4. Soloist: TD-PSOLA

`src/engines/soloist.rs`

- **Pitch** — YIN ([de Cheveigné & Kawahara 2002](sources.md)) บนผลรวมแชนเนล คำนวณ difference function ด้วย FFT autocorrelation หนึ่งครั้ง บน grid ตำแหน่ง source ที่ตายตัว (ผลไม่ขึ้นกับ block) · refine แบบ parabolic บน difference function ดิบ · hysteresis ของ voicing · กัน octave jump
- **Epoch** — voiced run ใหม่เริ่มที่จุดพลังงานสูงสุดของคาบ (glottal pulse) grain ถัด ๆ ไปต่อจากตัวก่อนทีละ *จำนวนเต็มคาบ* ที่ใกล้ map ที่สุด แล้วตรึง lag ด้วย waveform similarity · pulse อยู่กลาง grain (window = 1) เสมอ
- **Grain** — Hann ยาวสองคาบ วางห่างหนึ่ง *คาบขาออก* (`T/p`) ที่ตำแหน่งเศษส่วนจริง (ปัดเป็น sample ทำ pulse train สั่น ±½ sample = เพี้ยน 3 cents วัดได้) · overlap-add **ไม่ normalize ด้วยผลรวม window** แบบ TD-PSOLA ดั้งเดิม: pulse คงระดับเดิม เสียงสูงมี pulse ถี่ขึ้น เสียงต่ำมีช่องว่าง (normalize แล้ววัดได้ peak ตก 2.4 dB เพราะหาร pulse ด้วย p)
- **Unvoiced** (ลมหายใจ, s, sh) — grain สั้น 6 ms วางต่อกันพอดี อ่านตาม map ตรง ๆ
- **Formant** — ฟรีตามธรรมชาติ: อ่าน grain ที่อัตรา `f` (1 = คง formant, p = ตาม pitch)
- **Transient** — ใช้ lock ตัวเดียวกับ Elastic (หน้าต่าง 21 ms)

## 5. Real-time

`src/stream.rs` — โครงเดิม (SPSC ring, prefetch worker, pull adapter, underrun policy, metrics) บวก:

| | |
|---|---|
| **retarget ก่อนเสมอ** | `StreamHandle::set_plan` ถ้าโหมด/แชนเนล/rate/window/transient เหมือนเดิม worker สร้าง `LiveUpdate` (map สองชุด + pitch + formant) callback สลับเข้า engine ที่กำลังเล่น — ไม่สร้าง voice ใหม่ ไม่ crossfade · ถ้า voice ยังรออยู่ใน handoff slot worker แก้ให้เองตรงนั้น |
| สร้างใหม่เมื่อจำเป็น | เปลี่ยนโหมด หรือเปิด/ปิด transient → voice ใหม่ + crossfade แบบเดิม |
| metrics | `retargets` (เปลี่ยนแบบ in-place) แยกจาก `swaps` (สร้างใหม่) |
| prefetch | 0.5 → **1.0 s** — look-ahead ของ transient lock ต้องการ ~0.25 s และมากขึ้นเมื่อบีบเวลา |
| บั๊กที่เจอ | voice ป้อน input ทีละ 4096 frame ไม่สนใจ `max_block` ของ plan → plan ที่ compile ด้วย max_block เล็กกว่า fail เงียบ ๆ (เงียบไปทั้ง stream) · แก้แล้วและมี test |

demo ส่ง plan ทุก 40 ms ระหว่างลาก slider/anchor (เดิม 120 ms และรอให้หยุดลากก่อน) เพราะแต่ละครั้งแค่ retarget

## 6. ผลวัด

`solfege selftest` ผ่านทุก gate (identity bit-exact, M frames ทุกโหมด, block invariance −inf dBFS, +7 st = −0.03 cents, stereo inversion −1.0000, hard anchor 0.062 ms, typed rejections, empty source)

### 6.1 Attack — จุดที่ต่างจากเดิมมากที่สุด

`solfege quality` fixture กลอง (noise burst + ตัวโทน ทุก 250 ms), 48 kHz · "เดิม" คือ Polyphonic ตามตัวเลขที่บันทึกไว้ในเอกสารรอบก่อน

| | เดิม (Polyphonic) | **Elastic Pro** | Elastic Efficient | Rhythmic | Soloist |
|---|---|---|---|---|---|
| α 1.0, +3 st — rise (source 1.19 ms) | 2.36 ms | **1.04** | 1.04 | 1.04 | 1.45 |
| α 1.0, +3 st — pre-echo | −75.8 dB | **−120** | −120 | −120 | −120 |
| α 1.5 — rise | 4.59 ms | **1.19** | 1.19 | 1.19 | 1.07 |
| α 1.5 — pre-echo | −52.3 dB | **−120** | −120 | −120 | −120 |
| α 1.5 — onset error | 14.4 ms | **3.49** | 1.16 | 1.47 | 1.68 |
| α 1.5, −5 st — rise / pre-echo | – | **1.35 ms / −120** | 1.35 / −120 | 1.35 / −120 | 1.77 / −120 |

(−120 dB คือพื้นของตัววัด = ไม่มีพลังงานก่อน attack ที่วัดได้ · onset error วัดจากยอด envelope เทียบ `α × ตำแหน่งยอดใน source` — lock วาง *จุดเริ่ม* attack ตรง `W(o)` แล้วเล่นตัว attack ที่ความเร็วเดิม ยอดจึงห่างจาก `α×` อยู่ `(α−1) × ระยะจากจุดเริ่มถึงยอด` ซึ่งถูกต้องตามตั้งใจ · rise ที่ −5 st ยาวขึ้นเพราะ resample ลงทำให้ attack ช้าลง 1/p จริง)

### 6.2 ความนิ่งของโทน

`solfege stability --semitones 3` (cents rms ของ fundamental / dB rms ของ envelope):

| engine | 55 Hz | 82.5 Hz | 110 Hz | 220 Hz | 440 Hz |
|---|---|---|---|---|---|
| เดิม Polyphonic | 0.37 | 0.22 | 0.23 | 0.18 | 0.18 |
| **Elastic Pro** | **0.11** / 0.20 dB | **0.04** / 0.11 | **0.01** / 0.06 | **0.00** / 0.05 | **0.00** / 0.02 |
| Elastic Efficient | 0.10 / 0.20 | 0.04 / 0.12 | 0.01 / 0.05 | 0.00 / 0.04 | 0.00 / 0.03 |
| Soloist | 0.12 / 0.19 | 0.07 / 0.12 | 0.09 / 0.05 | 0.01 / 0.04 | 0.00 / 0.02 |

`solfege quality` แถวอื่น (Elastic Pro): vowel 130 Hz 0.01 cents / 0.10 dB · reverb tail roughness 0.06 → 0.07–0.09 dB · bass-under-drums 1.3 cents (+3 st), 1.5 (α 1.5), 3.5 (α 1.5 −5 st) — เดิม 2.47 · stereo: identical บิตเดียวกัน, inverted −1.000, mic delay 24 → 24 frames เมื่อไม่ย้าย pitch

### 6.3 Formant และ pitch (vowel 130 Hz, formant 730/1090/2440 Hz)

| | f0 (ต้องการ 173.63 / 97.46 Hz) | ศูนย์กลางพลังงาน 300–1400 Hz (source 661 Hz) |
|---|---|---|
| Elastic Pro +5 st preserve / follow | 173.64 / 173.64 | **672** / 765 |
| Elastic Pro −5 st preserve / follow | 97.45 / 97.45 | **648** / 529 |
| Soloist +5 st preserve / follow | 173.66 / 173.65 | 609 / 765 |
| Soloist −5 st preserve / follow | 97.46 / 97.46 | 666 / 529 |

(ศูนย์กลางเป็นตัววัดหยาบ: harmonic ชุดใหม่สุ่มตัวอย่าง envelope เดิมคนละจุด จึงขยับได้ ±8% แม้ envelope จะเหมือนเดิม — Soloist ซึ่งคง formant โดยโครงสร้างก็ขยับ −8%)

### 6.4 ต้นทุน

`solfege bench --block 256` (48 kHz, deadline 5.333 ms, offline path บนเครื่องพัฒนา):

| engine | α 1.5: p99 / max | α 1, +5 st: p99 / max | เดิม p99 (α 1.5) |
|---|---|---|---|
| Elastic Pro | 0.46 / 1.12 ms (8.7%) | 0.58 / 1.01 ms (10.8%) | Polyphonic 0.53 ms, Hybrid 1.00 ms |
| Elastic Efficient | 0.40 / 0.75 ms (7.4%) | 0.70 / 0.84 ms (13.1%) | – |
| Rhythmic | 0.15 / 1.46 ms (2.8%) | 0.46 / 2.76 ms (8.6%) | Percussive 0.59 ms |
| Soloist | 0.26 / 0.68 ms (4.9%) | 0.23 / 0.47 ms (4.4%) | Monophonic 0.20 ms |
| Varispeed | 0.22 / 0.39 ms (4.1%) | – | 0.12 ms |

ทุกตัวต่ำกว่าเป้า p99 ≤ 50% ของ deadline และไม่มี call ไหนเกิน deadline · max เกิดตอน warm-up ของเฟรมแรก ซึ่งใน stream worker ทำให้ก่อนส่งมอบ voice

### 6.5 Live (headless, `tests/stream.rs`)

240 callback × 512 frames, เปลี่ยน length/pitch ทุก 30 callback (6 ครั้ง, รวม −3 ถึง +5 st, ×0.7 ถึง ×1.6): Elastic Pro, Elastic Efficient, Soloist — **retarget ทุกครั้ง, สร้าง engine ใหม่ 0 ครั้ง, underrun 0, error 0, ไม่มี block ไหนตกเกิน 4 dB จาก median** และ pitch หลังเปลี่ยนตรงภายใน 10 cents

## 7. สิ่งที่ลองแล้วไม่ใช้

1. **Pitch shift ในโดเมนความถี่** (แบบ Laroche–Dolson / Signalsmith: ย้าย region รอบ peak แบบ rigid แล้วต่อ phase ด้วย heap) — ข้อดีคือไม่มี resampler แต่ attack กระจาย: สเปกตรัมของ hit ถูกแบ่งเป็นแถบที่เลื่อนคนละระยะ แต่ละแถบเป็น bandpass ของ hit ที่กระจายเวลา ~17 ms · ไล่แก้ไปสี่ชั้น (group-delay correction ระหว่าง region, integer bin shift, attack seed, per-bin attack flag) impulse ออกมาสมบูรณ์ (−96 dB) แต่ drum hit จริงยัง pre-echo −30 dB · stretch-then-resample ได้ −120 dB ทันที จึงเลือกทางนี้ (ตรงกับที่ [research.md §4.2](research.md) บันทึกว่า zplane และ Rubber Band ทำ)
2. **Interpolate bin แบบเศษส่วน** (cubic) ใน pitch map — ทำ time-aliasing ในเฟรม ปลาย attack วนมาโผล่ต้นหน้าต่าง
3. **Lock ครึ่งหน้าต่าง** (¾ ของ half-window) — pre-echo −37 dB ที่ α 1.5 · เต็มหน้าต่างได้ −72 dB แล้วหลัง attack seed ได้ −117 dB
4. **Normalize PSOLA ด้วยผลรวม window** — pulse level ตก 1/p (−2.4 dB ที่ +5 st)
5. **Cepstral envelope ธรรมดา** — formant ขยับไปได้แค่ครึ่งทาง (661 → 486 Hz เมื่อควรอยู่ที่ ~610) · true envelope + lifter ตาม pitch แก้

## 8. Tests (48)

- `tests/contract.rs` (30) — exact M frames ทุกโหมดทุก ratio · ทุก ratio × transposition จบได้ · block invariance (1…4096 + random) · silence · empty/1-frame · typed errors · no-progress state · drain · seek · reset = prepare · seek + transpose ยังผลิตต่อ · steady tone ไม่หายใจ · **เบส 55/82.5 Hz ไม่แกว่ง** · **attack: rise, pre-echo < −60 dB, ลงตรง map ทุก hit** · transient เปิด/ปิดไม่เปลี่ยนระดับ · pitch ตรง · varispeed · **formant ตาม policy** · stereo · hard anchor · **retarget กลางทาง** · compiler rejects · **document เก่าเปิดได้** · group · seeded FX · Auto · ไม่ normalize
- `tests/stream.rs` (2) — เล่นผ่าน callback จริง (ไม่มี device) ลาก slider ทุก 30 block: **retarget ≥5 ครั้ง, rebuild 0, underrun 0, ไม่มี block ไหนตกเกิน 4 dB, pitch หลังเปลี่ยนตรง** · เปลี่ยนโหมด = crossfade หนึ่งครั้ง
- `tests/mapping.rs` (14), unit tests ของ kernel (2)

## 9. ที่ยังไม่มี / ยังไม่ดี (พูดตรง ๆ)

- **ยังไม่มีการฟังแบบ blind** ตัวเลขทั้งหมดเป็น correctness และ diagnostic บน fixture สังเคราะห์
- **demo GUI คอมไพล์ในเครื่อง Linux ของ session นี้ไม่ได้** (eframe ใน Cargo.toml ปิด feature ของ platform, cpal ต้องการ ALSA) — แก้ตาม API ของ egui 0.31 แล้ว แต่ต้อง build บน Windows เพื่อยืนยัน
- **Rhythmic ไม่เหมาะกับเบส** — หน้าต่าง 21 ms แยก partial ของเบสไม่ออก (55 Hz แกว่งหลาย cents) ตามที่ตั้งใจ: Auto ส่งเนื้อเสียงที่มีเบสไป Elastic Pro
- **Soloist ต้องเป็นเสียงเดี่ยวจริง** คอร์ดหรือ reverb หนักทำให้ YIN สับสน → ใช้ Elastic Pro
- onset ที่ถี่กว่า span ของ lock (~120–300 ms ตาม ratio) จะเสีย lock ตัวหลัง — attack ยังลงตรง map แต่คมน้อยลง
- mic delay ถูกหารด้วย `p` เมื่อย้าย pitch (สมบัติของ stretch-then-resample ไม่ใช่การตัดสินใจแยกแชนเนล)
- multi-resolution / note editing (M6) / disk-backed reader / loop / group render — ยังไม่ทำ
