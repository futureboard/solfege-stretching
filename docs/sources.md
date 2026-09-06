# Sources & Evidence Register

[สารบัญ](README.md) · ตรวจค้น **2026-09-06**

แหล่งที่ใช้เป็นคู่มือผู้ผลิต SDK docs โค้ดจากผู้พัฒนา และบทความวิจัย ไม่ใช้ความเห็นใน forum เป็นหลักฐานอัลกอริทึมภายใน วันที่ค้นไม่ใช่วันเผยแพร่ และหน้าเว็บที่ไม่ผูกรุ่นไม่ใช่หลักประกันว่าเป็น implementation ของทุก version

## Apple

<a id="a1"></a>
### A1 — Flex Time

[Apple: Flex Time algorithms and parameters](https://support.apple.com/guide/logicpro/flex-time-algorithms-and-parameters-lgcpa77a4a3f/mac)

หน้า current guide และ [snapshot 10.7](https://support.apple.com/en-mide/guide/logicpro/lgcpa77a4a3f/10.7/mac/11.0) ระบุหกโหมด, Automatic และกลไก Slicing/Rhythmic/Polyphonic/Tempophone/Speed ไม่เปิดเผยสูตร Monophonic หรือ FFT implementation

<a id="a2"></a>
### A2–A3 — Flex Pitch

- [Apple: Flex Pitch algorithm and parameters, 10.7](https://support.apple.com/en-gb/guide/logicpro/lgcpba8e3301/10.7/mac/11.0) — track controls; ความหมาย Formant Track/Shift เป็นของ Apple โดยเฉพาะ
- [Apple: Edit pitch and timing with Flex Pitch, 10.7](https://support.apple.com/en-ie/guide/logicpro/lgcpc53e6bef/10.7/mac/11.0) — note timing และ hotspots

ใช้ยืนยันพฤติกรรม editor ไม่ยืนยัน PSOLA, pitch detector หรือ polyphonic note separation

## zplane

<a id="z1"></a>
### Z1 — Technology overview

[zplane Licensing: Technology — ELASTIQUE](https://licensing.zplane.de/technology)

ส่วน ELASTIQUE: PRO/EFFICIENT/TUNE, features และ comparison table หน้าเดียวกันมีคำกล่าว real-time แบบรวมกับตาราง TUNE ที่ต่างกัน จึงไม่ถือว่าทั้งสามมี raw-streaming contract เดียวกัน รายชื่อ DAW users ระบุบาง version เก่า ไม่ใช้ยืนยัน backend รุ่นปัจจุบันทั้งหมด

<a id="z2"></a>
### Z2 — Pro SDK

[ELASTIQUE PRO 3.3.7 SDK documentation](https://licensing.zplane.de/uploads/SDK/ELASTIQUE-PRO/V3/manual/elastique_pro_v3_sdk_documentation.pdf)

ส่วน 1.1–1.3: Pro/Efficient/SOLOIST, speech mode, infiniStretch/Hold และ synchronization; PDF physical pages 4–8 โดยประมาณ (เลขพิมพ์ต่างจาก page index) Public interface มากกว่าสูตรแกนกลาง

<a id="z3"></a>
### Z3 — Efficient SDK

[ELASTIQUE EFFICIENT 3.3.7 SDK documentation](https://licensing.zplane.de/uploads/SDK/ELASTIQUE-EFF/V3/manual/elastique_efficient_v3_sdk_documentation.pdf)

ส่วน Pitch synchronization อธิบาย resampler + stretch engine และผลด้าน timing เมื่อ pitch เปลี่ยน ไม่ใช่หลักฐานว่า native realtime latency เป็นศูนย์

## Avid

<a id="p1"></a>
### P1 — Elastic Audio framework

[Avid: Elastic Audio Plug-Ins, Pro Tools First 12.0](https://apps.avid.com/proToolsFirstHelp/version12.0/enu/Pro%20Tools%20First%20Help/Proc2.ElasticAudio.37.18.html)

เป็นฐาน **legacy** เรื่อง engine selector และ real-time/rendered ไม่ใช่รายชื่อปัจจุบันที่สมบูรณ์

<a id="p2"></a>
### P2 — élastique added in 2023.3

[Avid: What’s new in Pro Tools 2023.3](https://prod-werp.avid.com/resource-center/2023/03/whats-new-in-pro-tools-20233)

ประกาศเพิ่ม élastique Pro V3 ในทุก tier ณ release นั้น พร้อมคำกล่าว timing, phase coherence และ formant-preserving pitch shift แบบ real-time ไม่ใช้รับรอง feature entitlement ของทุก edition หลังจากนั้น

<a id="p3"></a>
### P3 — Polyphonic

[Avid: Polyphonic Plug-In](https://apps.avid.com/proToolsFirstHelp/version12.0/enu/Pro%20Tools%20First%20Help/Proc2.ElasticAudio.37.20.html)

รองรับ complex material และ clip-based pitch shift; ไม่บอกสูตร core

<a id="p4"></a>
### P4 — Rhythmic

[Avid: Rhythmic Plug-In](https://apps.avid.com/proToolsFirstHelp/version12.0/enu/Pro%20Tools%20First%20Help/Proc2.ElasticAudio.37.23.html)

เหมาะ material มี attack ชัดและรองรับ clip-based pitch shift

<a id="p5"></a>
### P5 — Monophonic

[Avid: Monophonic Plug-In](https://apps.avid.com/proToolsFirstHelp/version12.0/enu/Pro%20Tools%20First%20Help/Proc2.ElasticAudio.37.25.html)

วิเคราะห์ pitch เพิ่ม แต่ไม่รองรับ Elastic Audio pitch shift ตามคู่มือรุ่นนี้; ความต่างระหว่าง analysis กับ capability สำคัญต่อการออกแบบ

<a id="p6"></a>
### P6 — Varispeed

[Avid: Varispeed Plug-In](https://apps.avid.com/proToolsFirstHelp/version12.0/enu/Pro%20Tools%20First%20Help/Proc2.ElasticAudio.37.26.html)

ผูก time กับ pitch แบบ tape

<a id="p7"></a>
### P7 — X-Form

[Avid: X-Form Plug-In (Rendered Only)](https://apps.avid.com/proToolsFirstHelp/version12.0/enu/Pro%20Tools%20First%20Help/Proc2.ElasticAudio.37.27.html)

ระบุ iZotope Radius และความต่างระหว่าง Elastic Audio version กับ standalone AudioSuite ไม่ใช้คำว่า highest quality ของผู้ผลิตเป็นผลทดสอบเปรียบเทียบ

## Steinberg

<a id="s1"></a>
### S1 — élastique variants

[Cubase Pro 15.0: élastique](https://www.steinberg.help/r/cubase-pro/15.0/en/cubase_nuendo/topics/time_stretch_and_pitch_shift_algorithms/time_stretch_and_pitch_shift_algorithms_elastique_r.html)

ตรวจเทียบ [Nuendo 15.0: élastique](https://www.steinberg.help/r/nuendo/15.0/en/cubase_nuendo/topics/time_stretch_and_pitch_shift_algorithms/time_stretch_and_pitch_shift_algorithms_elastique_r.html?contentId=orgphZCiAovCy6lgsuAgfw) ด้วย: PRO/PRO Formant/efficient และ Time/Pitch/Tape

<a id="s2"></a>
### S2 — Standard presets

[Cubase Pro 15.0: Standard](https://www.steinberg.help/r/cubase-pro/15.0/en/cubase_nuendo/topics/time_stretch_and_pitch_shift_algorithms/time_stretch_and_pitch_shift_algorithms_standard_r.html?contentId=jzf9eHGKm8ABCVh_r6UHBw)

เจ็ด presets และ Custom Grain Size/Overlap/Variance ไม่บอก numerical defaults ของแต่ละ preset

<a id="s3"></a>
### S3 — Tempo matching / Free Warp

- [Cubase Pro 15.0: Tempo Matching Audio](https://www.steinberg.help/r/cubase-pro/15.0/en/cubase_nuendo/topics/sample_editor_tempo_matching_audio/sample_editor_tempo_matching_audio_c.html)
- [Cubase Pro 15.0: Correcting Timing with Free Warp](https://www.steinberg.help/r/cubase-pro/15.0/en/cubase_nuendo/topics/sample_editor_tempo_matching_audio/sample_editor_tempo_matching_audio_free_warp_timing_correcting_t.html)

แยก tempo/grid editing จาก signal synthesis

<a id="s4"></a>
### S4 — Phase-coherent groups

[Cubase Pro 15.0: Group Editing Mode](https://www.steinberg.help/r/cubase-pro/15.0/en/cubase_nuendo/topics/parts_events/parts_and_events_group_editing_mode_c.html)

ยืนยัน common boundaries/warp marker requirements และเป้าหมาย multimicrophone coherence ไม่เปิดเผย phase algorithm

<a id="s5"></a>
### S5 — VariAudio engine binding

[Cubase Pro 15.0: Algorithm Presets (Japanese)](https://www.steinberg.help/r/cubase-pro/15.0/ja/cubase_nuendo/topics/sample_editor_tempo_matching_audio/sample_editor_tempo_matching_audio_algorithm_presets_c.html)

ระบุ Standard–Solo ใช้อัตโนมัติกับ VariAudio; อ่านร่วมกับ [Operation Manual 9.5.40 (historical)](https://www.steinberg.help/api/khub/documents/NaAQ40rYC6KpEm50TMjbKw/content) เพื่อ cross-check ชื่อ ไม่ใช้คู่มือเก่าอ้างว่าทุก feature ยังเหมือนเดิม

## Additional products / open implementations

<a id="b1"></a>
### B1 — Ableton Live 12

[Ableton: Audio Clips, Tempo, and Warping, §9.3](https://www.ableton.com/en/live-manual/12/audio-clips-tempo-and-warping/)

Beats/Tones/Texture/Re-Pitch/Complex/Complex Pro; UI behavior ไม่เปิดเผย backend version

<a id="r1"></a>
### R1 — Rubber Band R2 internals

[Breakfast Quay: Technical notes](https://breakfastquay.com/rubberband/technical.html)

ระบุชัดว่าเนื้อหาจำกัด R2: phase vocoder, reset, adaptive stretching, lamination และ resampling

<a id="r2"></a>
### R2 — Rubber Band engine selection

[Official Rubber Band repository](https://github.com/breakfastquay/rubberband)

README อธิบาย R2/Faster และ R3/Finer; source repository เป็น primary source แต่การศึกษานี้ไม่ได้ audit implementation R3 ทั้งหมด

<a id="r3"></a>
### R3 — Rubber Band integration

[Breakfast Quay: Integration notes](https://breakfastquay.com/rubberband/integration.html)

variable frame counts, pull processing, padding/delay และ option recommendations ไม่ใช่ API ของ Solfege

<a id="t1"></a>
### T1 — SoundTouch

- [SoundTouch README, §3.3](https://soundtouch.surina.net/README.html)
- [Olli Parviainen: Time and pitch scaling basics](https://www.surina.net/article/time-and-pitch-scaling.html)

WSOLA-like time domain และ resampling; latency และ interpolation descriptions ผูกกับ implementation ที่เอกสารกล่าว ไม่ถือเป็น best-practice ทุกกรณี

<a id="m1"></a>
### M1 — Melodyne 5

[Celemony: Audio characteristics and algorithms](https://helpcenter.celemony.com/M5/doc/melodyneStudio5/en/M5tour_AudioAlgorithms)

Universal, Percussive, Percussive Pitched, Melodic, Polyphonic Sustain/Decay และการแก้ detection; ฟีเจอร์ขึ้นกับ edition

<a id="x1"></a>
### X1 — ZTX

[Zynaptiq: ZTX Time Stretching & Pitch Shifting](https://www.zynaptiq.com/ztx/)

adaptive wavelet claim, Core/FX/Retune และ formant/multichannel features; ไม่ใช่ algorithm specification

<a id="x2"></a>
### X2 — Pitch ’n Time

[Serato: Features of Pitch ’n Time Pro](https://support.serato.com/hc/en-us/articles/202523160-What-are-the-features-of-Serato-Pitch-n-Time-Pro)

variable time/pitch mapping และ multichannel; internal algorithm ไม่เปิดเผยในหน้านี้

<a id="x3"></a>
### X3 — Paulstretch

[Nasca Octavian Paul: paulstretch_stereo.py](https://github.com/paulnasca/paulstretch_python/blob/master/paulstretch_stereo.py)

อ่านส่วน FFT magnitude, random phase, inverse FFT; เป็น branch URL ไม่ใช่ pinned commit จึงควร pin ก่อนใช้เป็น reproducible code benchmark

## Research

<a id="d1"></a>
### D1 — TSM review

[Driedger & Müller (2016), A Review of Time-Scale Modification of Music Signals](https://www.mdpi.com/2076-3417/6/2/57), DOI `10.3390/app6020057`

อ่าน [สำเนาบทความ PDF](https://sites.units.it/ramponi/teaching/DSP/materials/S03.4a/Driedger16_Review.pdf) เมื่อ publisher page โหลดไม่ได้ ใช้ภาพรวม OLA/WSOLA/PV/HPSS และ trade-offs; ไม่ใช่หลักฐาน implementation ของ DAW proprietary

<a id="d2"></a>
### D2 — TSM Toolbox / HPSS

[AudioLabs: TSM Toolbox](https://www.audiolabs-erlangen.de/resources/MIR/TSMtoolbox/)

ผู้วิจัยเผยแพร่ implementations และตัวอย่าง OLA/WSOLA/PV/identity phase locking/HPSS พร้อมอ้างงาน Driedger, Müller & Ewert (2014), Improving Time-Scale Modification of Music Signals Using Harmonic-Percussive Separation ให้ใช้ attribution ของบทความตามชื่อจริง เนื่องจากเลขอ้างอิงบนหน้าเว็บอาจสลับกับข้อความบรรยาย

<a id="d3"></a>
### D3 — Epoch synchronization

[Epoch-Synchronous Overlap-Add (ESOLA) for Time- and Pitch-Scale Modification of Speech Signals (2018)](https://arxiv.org/abs/1801.06492)

งานวิจัยเฉพาะ speech ไม่ใช่หลักฐานว่า PSOLA/ESOLA เหมาะกับ full mixes หรือถูกใช้ใน Flex Pitch

<a id="d4"></a>
### D4 — Noise and neural research

- [Noise Morphing for Audio Time Stretching (2023)](https://arxiv.org/abs/2312.14586)
- [Extreme Audio Time Stretching Using Neural Synthesis (2022)](https://arxiv.org/abs/2211.16992)

ใช้ยืนยันว่ามีการศึกษาส่วน noise และ neural synthesis; ยังไม่ได้ reproduce results จึงอยู่ backlog ไม่เป็น production choice

## สิ่งที่ยังไม่ยืนยัน

| ประเด็น | สถานะ / วิธีตรวจต่อ |
|---|---|
| Apple Monophonic/Flex Pitch core equations | ไม่เปิดเผยในคู่มือ; ห้ามอ้างเป็น PSOLA แบบยืนยัน |
| élastique psychoacoustics และ frame scheduler ทั้งหมด | Public SDK ไม่พอทำซ้ำ; ใช้ own DSP และ benchmark output |
| Avid legacy feature behavior ทุก edition ปัจจุบัน | pin รุ่น/edition และตรวจคู่มือ/แอปจริงก่อนทำ compatibility promise |
| คุณภาพเปรียบเทียบทุกเครื่องยนต์ | ต้องใช้ corpus และ protocol เดียวกัน; ยังไม่มีผลทดลอง |
| Latency/CPU ของ Solfege | ยังไม่มี implementation; ตัวเลขใน design เป็นเป้าหมาย |
| สิทธิ์นำ dependency/code/audio corpus มาแจก | ยังไม่ได้เลือก dependency หรือแจกโค้ด/เสียง; ตรวจ license ของรุ่นจริงใน implementation milestone |

เอกสารนี้เป็น research/design ไม่ใช่การประเมินเสรีภาพในการใช้สิทธิบัตรหรือข้อสรุปทางกฎหมาย
