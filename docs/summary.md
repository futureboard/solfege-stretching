# สรุปเอกสารทั้งชุด

สรุปจาก [README](README.md) · [research](research.md) · [dsp](dsp.md) · [system-design](system-design.md) · [validation](validation.md) · [sources](sources.md) · [implementation](implementation.md)
เอกสารต้นทางตรวจค้น 2026-09-06 · สรุปฉบับนี้ 2026-09-06 (อัปเดตสถานะหลัง implement)

## 1. โปรเจกต์นี้คืออะไร

ไลบรารี Rust สำหรับ **time stretching + pitch shifting + warp** ที่รองรับเสียงร้อง เครื่องดนตรีเดี่ยว กลอง และ full mix โดยเป็น **non-destructive**: ผู้ใช้แก้ anchor/โน้ตกี่ครั้งก็ได้ และ render จากไฟล์ต้นฉบับเสมอ

ลำดับการสร้าง: **offline WAV + CLI + measurement harness ก่อน** → แล้วค่อยเพิ่ม playback จากไฟล์ → แล้วค่อยเป็น note editor / GUI / plug-in (ซึ่งเป็นแค่ consumer ของไลบรารี ไม่ผูก DSP กับ UI framework)

**สถานะจริงของโค้ดตอนนี้ (อัปเดต 2026-09-28, Elastic rework):** ไลบรารี `solfege` มีโหมด **Elastic Pro, Elastic Efficient, Rhythmic, Soloist, Varispeed, Texture** + Auto/Bypass, plan compiler, analysis, WAV IO, stream ที่ **retarget engine ระหว่างเล่นได้โดยไม่สร้างใหม่**, CLI `solfege` และ demo egui · ชุดทดสอบ 48 ตัว · ทุก correctness gate ผ่าน · รายละเอียด การออกแบบ และผลวัดอยู่ใน [Implementation](implementation.md) · ส่วน §3–§8 ด้านล่างเป็นสรุปเอกสาร research/design เดิม ซึ่งยังเป็นกรอบที่ใช้อยู่ — ชื่อ engine ในตารางเก่า (WSOLA, Polyphonic, Hybrid, Percussive) ถูกแทนตาม [Implementation §0](implementation.md) · สิ่งที่ยังไม่มีคือ note synthesis (M6), group render และ listening test

## 2. ข้อเท็จจริงของ workspace ที่ตรวจแล้ว (ปิด TODO ของ system-design §1)

เอกสารสั่งให้ตรวจ workspace แม่ก่อนเริ่ม implement — ตรวจแล้วที่ `../Cargo.toml`:

| ค่า | ที่ inherit มา |
|---|---|
| `version` | `0.1.0` |
| `edition` | `2024` |
| `license` | `Apache-2.0` |
| resolver | `3` |
| membership | `solfege-stretching` เป็น member ของ workspace `W:\works\dsp` |
| workspace deps ที่มีให้ใช้แล้ว | `serde` (derive), `serde_json`, `fbmx-runtime`, `fa76` |

serialization ของ `EditDocument` ใช้ `serde`/`serde_json` จาก workspace · **dependency ที่เลือกจริงหลัง M0/M1:** `rustfft` + `realfft` เท่านั้นสำหรับ FFT (WAV IO และ resampler เขียนเอง ไม่พึ่ง crate นอก) ส่วน `eframe` + `cpal` ของ demo อยู่หลัง feature `demo` เพื่อไม่ให้ไลบรารีลาก GUI/audio driver ติดไป

## 3. Research — บทเรียนที่ใช้จริง (ไม่ใช่การลอกผลิตภัณฑ์)

ผลิตภัณฑ์ที่ศึกษา: Logic **Flex Time** (6 โหมด + Automatic) / **Flex Pitch**, zplane **élastique** (PRO / EFFICIENT / SOLOIST / TUNE), Pro Tools **Elastic Audio** (Polyphonic, Rhythmic, Monophonic, Varispeed, X-Form, + élastique Pro V3 ตั้งแต่ 2023.3), Cubase **AudioWarp** (élastique 9 combination + Standard 7 presets + VariAudio) และตัวเทียบ Ableton Warp, Rubber Band R2/R3, SoundTouch, Melodyne, ZTX, Pitch 'n Time, Paulstretch

ข้อสรุปที่นำมาใช้ออกแบบ:

1. **warp ≠ อัลกอริทึม** — warp คือการกำหนดว่าจุดไหนของไฟล์ไปอยู่เวลาไหน; อัลกอริทึมสังเคราะห์เป็นอีกชั้น ทุกเจ้าแยกสองชั้นนี้
2. **ไม่มีอัลกอริทึมเดียวที่ชนะทุกเนื้อเสียง** — ทุกผลิตภัณฑ์มีหลายโหมด เราจึงมีหลายโหมดใต้ time map เดียว
3. **แยกโหมดรักษาต้นฉบับออกจาก FX** (Tempophone/Texture/Paulstretch เป็น FX ไม่ใช่ fallback ของ Natural)
4. **pitch shift = time stretch + resampler** (ยืนยันจาก SDK ของ zplane และ Rubber Band R2) — ต้องออกแบบ clock ให้ชัดตั้งแต่ต้น
5. **analysis event ≠ user anchor** — ที่ detector เจอเป็น hint จนกว่าผู้ใช้จะ promote
6. **แต่ละ call ให้ output ไม่คงที่** (บทเรียนจาก Rubber Band integration) — API ต้องรายงาน consumed/produced จริง
7. **monophonic content ≠ mono channel** — เลือกวิธีจากเนื้อเสียง ไม่ใช่จำนวนแชนเนล

สิ่งที่ **ไม่ทราบและห้ามอ้าง**: สูตรภายในของ Apple Monophonic/Flex Pitch, psychoacoustic model และ frame scheduler ของ élastique, การจัดอันดับคุณภาพระหว่าง engine (ยังไม่มี A/B, ไม่มี benchmark, ไม่มีการอ่าน source proprietary)

## 4. DSP — แกนคณิตศาสตร์

**นิยามหลัก** (sample rate ภายในเท่ากันตลอด render):

- `t = W(s)` — forward time map; `alpha = dt/ds` (>1 คือยาวขึ้น); `v = 1/alpha`
- `p = 2^(semitones/12)` — pitch multiplier; `f = 2^(formant_semitones/12)` — ตัวคุมแยกจาก `p`
- source ที่ tempo เปลี่ยน ต้อง map ผ่าน beat: `source sample → source beat → dest beat/time → output sample`; **sample position คือ source of truth ของ DSP, beat อยู่ชั้น document**

**การประกอบ stretch + pitch:**

- constant: stretch ด้วย `alpha_internal = alpha * p` แล้ว resample ที่ rate `p`
- variable automation: **ห้ามคูณ `alpha*p` ข้าม clock ตรง ๆ** — ต้องผ่าน intermediate coordinate `u(t) = ∫p(q)dq`, `U(s) = u(W(s))`, `U'(s) = p(W(s))·W'(s)` แล้ว `y(t) = resample(z, u(t))` ด้วย fractional accumulator เดียว และ control ต้องเลื่อนตาม logical timestamp ไม่ใช่เวลาที่ callback มาถึง

**ตระกูลอัลกอริทึมกับบทบาทในเรา:** Resampling→Tape, Slicing→drums, OLA→baseline อ้างอิง, WSOLA→Monophonic baseline, PSOLA/epoch→อนาคต, Phase vocoder→Polyphonic, HPSS→Hybrid experimental, Granular/random-phase→Texture FX, Neural→ไม่อยู่ MVP

**WSOLA ข้อกำหนดของเรา:** offset เดียวทุกแชนเนล (score ถ่วงด้วยพลังงาน ไม่ downmix), จำกัด displacement และแก้ drift ก่อนถึง anchor ถัดไป, ห้าม candidate ข้าม protected transient, ไม่ normalize ด้วยพลังงานใกล้ศูนย์ — ค่าเริ่มต้นทดลองที่ 48 kHz: frame 20–40 ms, overlap 5–15 ms, search ±5 ms

**Phase vocoder:** `delta = principal_arg(phi_m - phi_{m-1} - omega_k·Ha_m)`, `omega_hat = omega_k + delta/Ha_m`, `theta_m = theta_{m-1} + omega_hat·Hs_m` — เริ่มที่ window 2048 / hop 512 @48k, ต้อง `Ha_m > 0` เสมอ (freeze เป็น state แยก ห้ามใส่ `Ha=0`), ต่อด้วย peak phase locking, transient protection, และระวัง DC/Nyquist ของ real FFT

**Transient-constrained mapping:** `alpha_sustain = (L_out − P)/(L_in − P)` ใช้ได้เมื่อ `L_in > P` และ `L_out > P` เท่านั้น — ถ้า output สั้นกว่าผลรวม protected attack ต้องส่ง `ConstraintConflict` **ห้าม** ratio ติดลบหรือตัด hit ทิ้งเงียบ ๆ

**Formant:** `A(omega) = E(omega)·R(omega)`; ชดเชยด้วย `gain(omega) = E(omega/f) / max(E(omega/p), eps)` — เป็นแบบประมาณสำหรับทดลอง เริ่มกับ voiced mono เท่านั้น; policy: `Preserve ⇒ f=1`, `FollowPitch ⇒ f=p`, `Shift(st) ⇒ f=2^(st/12)`

## 5. System design — สัญญาที่ต้องไม่ผิด

**Pipeline:** immutable source + edit document → analysis cache (versioned) → **plan compiler** → immutable render plan → engine → pitch resampler/envelope → delay alignment → output
Tape ไม่ผ่าน pitch-independent stretch, Bypass copy ตรง ๆ — ไม่ใช่ทุก engine ผ่านทุก node

**โมดูล:** `audio`, `document`, `analysis`, `mapping`, `plan`, `dsp`, `engines`, `runtime`, `render`, `cache`, `cli` — เริ่มเป็น module ใน crate เดียว ยังไม่ต้องแตก crate

**Time-map contract (สำคัญที่สุด):** anchor ใช้ **sample boundary** ไม่ใช่ index ของ sample สุดท้าย; ต้อง monotonic เข้ม (`s_{i+1}>s_i` และ `t_{i+1}>t_i`), ต้องมี `(0,0)` และ `(N,M)`, ใช้ `f64` interpolate แต่ endpoint ล็อกเป็น integer (ห้ามสะสม rounding ต่อ block), ไม่ extrapolate นอก source

**Internal ratio gate:** `alpha=2, p=2` แปลว่าภายในต้อง stretch 4 เท่า — compiler ต้องตรวจทั้ง user-facing และ internal ratio และ **reject combination ได้แม้แต่ละตัวอยู่ในช่วงที่รองรับ**

**Processing API:** `prepare` / `reset` / `process` / `latency` โดย
`ProcessReport { consumed_frames, produced_frames, state, output_start_frame }`,
`ProcessState = NeedInput | HaveOutput | Draining | Finished`
กติกา: caller ต้อง re-submit input ที่ยังไม่ consumed; หลัง EOF อาจผลิต tail หลายรอบ; no-progress call ต้องบอกว่ารออะไร (ห้าม spin); malformed/NaN เป็น typed error ไม่ panic; buffer มี upper bound จาก config ไม่โตเอง
lifecycle: `Unprepared → Ready → Running → Draining → Finished`

**Real-time:** callback **ห้าม** allocate/dealloc, lock, IO, blocking log, สร้าง FFT plan — และการสลับ `Arc` ต้อง retire ให้ worker ทำลาย ไม่ให้ last-drop คืน memory ใน callback
`LatencyInfo` แยกสี่ค่า: `lookahead_input`, `startup_padding_input`, `presentation_delay_output`, `tail_output` — ห้ามยุบเป็นเลขเดียว
**ข้อจำกัดที่ต้องยอมรับ:** live mic + `alpha>1` สะสม backlog ไปเรื่อย ๆ — รุ่นแรก "real-time" หมายถึงเล่นไฟล์ที่ prefetch ได้เท่านั้น ไม่สัญญา indefinite live stretch
underrun → fade สั้นไป silence + นับ counter; **ห้าม**สลับเป็น dry signal และ**ห้าม**เปลี่ยนอัลกอริทึมโดยผู้ใช้ไม่รู้

**Stereo/group:** แยกสี่เรื่อง — shared time map, shared transient/WSOLA decision, phase relation ใน spectral engine, physical mic delay; ห้าม reset phase หรือเลือก peak แยกอิสระต่อแชนเนล; compiler reject group ที่ rate/origin ไม่ตรง (ไม่ auto-align แอบเปลี่ยน delay)

**Cache:** `analysis_key = hash(source_content, rate, layout, analyzer_version, settings)`; `render_key = hash(analysis_key, canonical_edit_document, engine_version, quality_profile, output_format, seed)` — ใช้ content hash ไม่ใช่ path/mtime; เขียนแบบ temp + atomic rename

**Offline exactness:** render → drain → ตัด startup padding ตาม timestamp → output ต้องได้ `M` frames พอดี; การ crop/pad ตอนจบใช้แก้ rounding ที่นิยามไว้เท่านั้น **ห้าม**ใช้ปิด cumulative timing bug; ไม่ auto-normalize/limit (รายงาน peak ให้ผู้ใช้เลือก)

## 6. Validation — เกณฑ์ที่ต้องผ่าน

**Correctness gates:** identity bypass sample-exact / output = `M` frames เสมอ / inverse round-trip ≤ 0.5 frame / hard anchor scheduler error ≤ 1 frame / steady sine pitch error ≤ 1 cent (median) / ไม่มี NaN-Inf จาก finite input / **block invariance: RMS diff ≤ −100 dBFS** ข้าม block size 1, 17, 64, 127, 256, 1024 / drain ไม่ซ้ำ-ไม่ค้าง / invalid edit เป็น typed error ก่อน render

**Corpus:** analytical (silence, impulse, sine 55/110/440, chirp), drums, tuned percussion, voice (ไทย/อังกฤษ + vibrato + sibilants), mono instrument, polyphonic, mix, stereo/group fixtures (identical / inverted / delayed), edges (empty, 1 frame, สั้นกว่า window, clipped, NaN)
**เก็บ tuning corpus แยกจาก held-out set**

**Parameter matrix:** `alpha` = 0.5/0.75/0.9/1/1.1/1.5/2 (stress 0.25, 4 รายงานแยก); pitch = −12/−7/−1/0/+1/+7/+12; rates 44.1/48/96k; operations start/short-EOF/drain/seek/loop/cancel/plan-swap/underrun — **ไม่ต้องคูณเป็น full Cartesian**

**Runtime gates:** 0 allocation หลัง prepare, ไม่มี blocking ใน callback, p99 ≤ 50% ของ block deadline (256/48000 = 5.333 ms ⇒ ~2.667 ms), memory bounded — แต่ยังไม่ได้เลือก reference machine จึงยังไม่ใช่คำสัญญาด้าน performance

**Listening:** blind + gain-matched, ให้คะแนนแยก attack / tonal stability / naturalness / stereo image / preference, pilot ≥5 คน, รายงานแพ้-เสมอ-ชนะแยกตามหมวดและ ratio; ABX บอกได้แค่ "ต่างไหม" ไม่ใช่ "ดีกว่าไหม"

## 7. Roadmap

| ขั้น | ส่งมอบ | ผ่านเมื่อ |
|---|---|---|
| **M0** ✅ | workspace ตรวจแล้ว, library/CLI, units, fixtures, map validation | build ผ่าน, mapping/property tests 14 ตัวผ่าน |
| **M1** ✅ | WAV offline IO, anti-aliased resampler, exact length (Bypass/Tape) | identity bit-exact, tape pitch-rate ผ่าน, endpoint exact |
| **M2** ✅ | WSOLA + shared-channel search + transpose composition | mono/stereo matrix ผ่าน · **ยังไม่มี listening report** |
| **M3** ✅ | Anchors, slicing/tail policy, protected-attack scheduler | hard anchor 0.062 ms, constraint conflict เป็น typed error |
| **M4** ✅ | STFT/PV + phase locking + transient handling | endpoint/stereo/pitch diagnostics ผ่าน · **ยังไม่มี A/B ฟัง** |
| **M5** ✅ | File playback, bounded buffers, seek, prefetch worker, live plan swap | เล่นผ่าน audio callback จริงครบทุกโหมด **0 underruns** และเปลี่ยน plan ระหว่างเล่นได้ · ยังไม่มี disk reader/loop |
| **M6** ◻ | Vocal editor core: F0 confidence, note edits, formant policy | analysis + data model มีแล้ว; **ยังไม่มี note synthesis path** |
| **M7** ◻ | Hybrid (HPSS) + multitrack coherence | hybrid engine มีแล้ว; ยังไม่พิสูจน์ว่าดีกว่า single-engine |

**แต่ละขั้นจบเมื่อมี implementation + evidence — ไม่ใช่มีชื่อโหมดใน enum**

## 8. ความเสี่ยงที่ต้องเฝ้าตั้งแต่บรรทัดแรก

1. transient protection ทำให้ map เป็นไปไม่ได้ → typed conflict ไม่ใช่ silent fix
2. pitch automation ทำ anchor drift → ต้องทดสอบการประกอบ intermediate/output clock
3. ตัดสินใจแยกต่อแชนเนล → shared decision state + phase fixtures
4. F0 ผิดในเสียงร้องมี reverb → confidence + manual correction
5. ratio สุดขั้วกิน CPU/RAM → validate capability ก่อน prepare

## 9. สิ่งที่งานนี้ยังไม่มี (พูดให้ตรง)

**มีแล้ว:** implementation ครบ M0–M5, correctness gates ที่รันจริง, runtime bench ทั้ง offline และใน audio callback จริง (ดู [Implementation §4](implementation.md))

**ยังไม่มี:** listening test ใด ๆ, การเทียบกับ Rubber Band / SoundTouch / DAW เชิงพาณิชย์, reference machine ที่ตกลงกันไว้, note synthesis (M6), disk-backed reader และ loop ในชั้น playback, group render และ cache บนดิสก์

ตัวเลข *คุณภาพเสียง* จึงยังไม่มีเลย และไม่มีการอ้างว่าเทียบเท่าผลิตภัณฑ์ใด ตัวเลขที่รายงานทั้งหมดเป็นเรื่อง correctness และ cost ซึ่งเป็นคนละเรื่องกับว่าเสียงดีไหม ตามที่ [validation.md §1](validation.md) แยกไว้

## 10. งานถัดไป

M0–M5 ทำเสร็จแล้ว ลำดับถัดไปตามความสำคัญ:

1. **Listening protocol** — เป็นสิ่งเดียวที่ขวางไม่ให้พูดอะไรเกี่ยวกับ *คุณภาพเสียง* ได้เลย (validation.md §5)
2. **M6 note synthesis** — `NoteEdit` / `DetectedNote` / F0+confidence / note segmentation มีครบแล้ว เหลือเส้นทางสังเคราะห์ที่นำ `NoteEdit` ไปใช้จริง
3. **WSOLA search cost** — แพงที่สุดในบรรดาโหมด ทั้งใน bench (p99 19% ของ deadline) และใน callback จริง (max 1.84 ms) ควรทำ correlation แบบ FFT-based หรือ decimate ก่อน search
4. **Disk-backed reader + loop** — prefetch worker วางโครงให้เปลี่ยนที่จุดเดียวได้แล้ว แต่ตอนนี้ source อยู่ใน RAM ทั้งก้อน
5. **Group render API** — `check_group` validate ได้แล้ว เหลือ render หลาย track ด้วย plan เดียวพร้อมกัน
