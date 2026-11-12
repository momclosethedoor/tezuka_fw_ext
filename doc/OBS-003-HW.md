# OBS-003-HW — аппаратная приёмка

2026-11-12, Pentium / Windows, один исполнитель. **BLOCKED; OBS-003 не закрыт.**
Реальный OBN1 источник распознан, selected=recorded=8, lost=0, но DMA-слоты
нарушают ABI: заголовок найден в слове 3552 вместо 0. До GOBS1 данные не дошли.
Экспериментальный агент завершён; **после физического питания проверенно
восстановлены PL-5b, штатный ELF, свежий спектр и real8. TX выключен.**
Единый каталог локальных доказательств: `C:/proj/pluto_ag/evidence/obs-003-hw/`.
Сырые данные, подключения и подробные логи остаются на Pentium.

## Реально использованные версии

| Компонент | Версия / SHA256 |
|---|---|
| OBS PL | `370d03a02fface2a287d8d7cafb6cee7422217ee`, A + PL-PREP, OBN1-r1 |
| system_top.bit | `311b49bc1d82646ad2f73d33ff475b304318b0e4b0d9049b83f31b087a6b19f9` |
| /tmp/OBS-003-PL.bit.bin | `089e9227e72593c546821ca79711caf46246fecff074f741925b7c783ed6ad12` |
| Первый ARM ELF | CI 43138510348/1, source `170747eed43d0156468a8c47814fb941f82d4ab3`; `7539195d9462b8704310f32c6c0a8149f727e1326ba9a6d7429d401f4f75f2d5` |
| ARM ELF аппаратного захвата | CI 43213696707/1, source `46d31bdc4389bd4f33c63f7e4fe9b230f3d27b6a`; **`d03d22c360d2eda399f27bd36d9e43a5d494cb89dca0f38ac8cee0802e3c9c7c`** |
| Firmware перед работой | `obs-003-arm` / `849ee875015730f60062fd95de736b7b713fad26`, clean; Maia prepared `c12ac2e3c6d9b6335d9f59628201e48d05125a51`, clean |
| pl_uart_obs.sh r1 | goofy_docs `3c1839018cd08f8f07be6c47d3a903798798f3fe`; `f9964280f463f545ac7f25d7f1a9e441a20a4f6c50a35e34c222dbcc7ef402b4` |
| Неизменный pl_uart_original.sh | `90c28dab015e0c225e2de49ec9a23111ad0047107eeb2cd2d51adc628bb20fe2` |
| Установленный ELF | `/usr/bin/maia-httpd`, `6e13d631f6e485bb11038799ff12d2c5130abc3220d23b6e42b2f95bec840596` |
| Установленный qspi-linux | `650f62111d41ef5f820f64c6e43e419a011504c889fbaf08e50b04cb189a2b0d`; совпал с прежним backup, повторная полная копия не делалась |
| Установленный PL payload из сохранённого FIT | `/images/fpga@1`, 1084944 bytes, `8ae94a8850425166938a339dcc7cfe9594b478ffe6e23cf8347de08f5ffa67b4` |

## UART и временная загрузка

Фактический физический **COM5**, `\Device\Serial2`, 115200 / 8N1, flow control off.
Проверено ответом `tty=/dev/ttyPS0`, kernel 6.12.77 и boot_id. COM8 — USB console,
для управления переходом не использовался. Первое открытие COM5 было отклонено:
Глеб закрыл терминал, после чего порт открылся. Один владелец — PowerShell
`evidence/obs-003-hw/uart-owner.ps1 -Port COM5`, .NET SerialPort.
Непрерывный побайтный транскрипт: `obs-003-hw/uart.log`, время команд:
`uart-events.log`. Файлы на Pentium, flush после каждого полученного блока.

`load`, `status`, `restore` выполнены в UART Linux shell. Скрипт r1 не изменялся;
старую Maia/контроллеры на OBN1 не возобновляли. Доставка файлов и readback — SSH.
Оба захода достигли `PROGRAMMED` / manager `operating` и `OBS_READY`; драйверы
PHY/ADC/DDS/DMA/UIO вернулись с прежней IIO-нумерацией.

Фактический DT: 0x16000000 / 0x40000, 8 × 32768, UIO 0x1000; expected drivers
совпали (`preflight.txt`). `flags=0` запускает full manager programming. Проверен
`drivers/fpga/zynq-fpga.c` закреплённого ADI kernel `bf1d49f17fa9e15bc40be2615ef67cdf023f660a`:
write_init утверждает AXI interface resets, выключает level shifters и выполняет
PCFG_PROG_B sequence. Это основание применимости согласованного полного manager
пути; busy=0 не использовался как доказательство AXI drain. SDR-only reset не вводился.
Прежнее общее требование нового DT/ядра/RAM-DFU не подтверждено: наблюдаемый
полный manager load/rebind работает с установленными драйверами и прежним DDR ring.
Формат данных после этого перехода проверку не прошёл; применимость процедуры
до OBS_READY не равна аппаратной приёмке размещения native DMA.

## Обнаруженные препятствия и исправления

Первый boot `d256de6a-a272-492c-a4d7-3fcc7be3c69b`: после OBS_READY установлен
статический RX: 30.72 Msps, 2.4 GHz, BW 18 MHz, manual gain 40 dB, TX powerdown=1,
gain -89.75 dB. Дополнительное диагностическое чтение `0x7c460020` до native
startup не вернулось; SSH оборвался, UART показал новую загрузку U-Boot/Linux.
Причина перезапуска не установлена. Это чтение нарушало порядок снятия reset
в handoff и удалено из локальной подготовки. Загрузчик не изменён; reboot-команд,
DFU или записи flash не было. Доказательства: `first-attempt.txt`, `uart.log`.

В UART подтверждён возврат установленного ELF/штатной Maia и новый boot
`b844898b-c282-4933-9e4c-8b206044953d`. Физическое выключение/включение
исполнителем не наблюдалось; Глеб разрешил считать этот возврат достаточным
для продолжения и велел продолжить без дополнительных процедур.

На втором заходе согласованный ELF отказал до UIO/DMA/claim marker:
`stop the stock DMA owner and its auto-restart before native startup`.
UART-проверка не нашла другого maia-httpd. Код Guard сканировал также `/proc/self`
и принимал себя за другого владельца. Правка ограничивает проверку другими
числовыми PID, сохраняет отказ для реального чужого процесса. Boot/capture guards,
счётчики и lifetime marker не отключались и не удалялись.

Изолированные ветки: firmware `obs-003-hw`, исходники Maia `obs-003-hw-native`.
Правка firmware `46d31bdc4389bd4f33c63f7e4fe9b230f3d27b6a`, Maia `8044959`.
ARM-only [CI 43213696707](https://github.com/momclosethedoor/tezuka_fw_ext/actions/runs/43213696707),
attempt 1, **success**: 19 Rust + 22 Python на каждой ОС, ARM compile/staging/
offline QEMU checks. Артефакты `artifacts/43213696707-attempt-1/`; manifest,
actual source map и все SHA256SUMS сверены. Доставленный в /tmp ELF повторно
хеширован; /usr/bin/maia-httpd не заменялся. Full firmware/PL сборка не запускалась.

## Конкретный аппаратный блокер

После правки guard новый ELF в первом настоящем source lifetime распознал
OBN1-r1 и прежнюю геометрию. Реальный boot_id `b844898b-c282-4933-9e4c-8b206044953d`,
capture UUID `0bb18111-bb67-4eaf-a6d5-f5abaf9b7a25`. RF readback: Fs 30720000,
LO 2400000000, BW 18000000, gain manual 40 dB, TX powerdown=1 / -89.75 dB.
Внешняя статическая пауза была установлена; минимальный settling не измерялся.

UART зарегистрировал `native hardware source READY`, затем через ~7 ms источник
снял validity. Snapshot: produced=2117, selected=8, recorded=8, lost=0,
last_slot=7, last_seq=2095, status=0. Produced — число окон с момента PL reset,
включая предшествующие/unselected; это не число отправленных наблюдений и не
полный 90-секундный замер. Запрошенный 90 s проход не состоялся. Читатели fast
и slow (20 ms/record) за 60 s не получили GOBS preamble/наблюдений и завершились
timeout. Выходные contention/client queue loss counters не получены.

Процесс после выхода источника ожидал UIO blocking read; наблюдались потоки
`futex_wait_queue` и `uio_read`. Это скрывает финальное сообщение ошибки main:
`App::run` запускает существующий InterruptHandler, хотя native completion
определяется счётчиками. Временный ELF идентифицирован по /proc/exe и SHA,
завершён TERM. Validity уже была 0, SDR-only reset не выполнялся, marker сохранён.
Процесс не был принудительно перезапущен. Это отдельное ограничение observability/
shutdown; механизм смещения DMA им не объясняется.

После остановки, без rearm и без второго DMA owner:

- `dd if=/dev/mem` вернул **Bad address**. `native-final-ring.bin` — пустой
  неудавшийся dump, не доказательство нулевой DDR и не capture.
- Через существующий `devmem` mmap прочитаны первые 32 u64 каждого слота и
  один полный неизменяемый слот 0. В нормативных header lanes всех 8 слотов — 0.
- В слоте 0 имеются 7369 ненулевых байт. Все mantissa/exponent/reserved-low
  проверки payload проходят; старшие halfwords заголовка находятся в словах
  **3552..3583**. Значит, это не пустой буфер. Независимый OBN1 decoder отвергает
  исходный слот: `native ABI/header constants`.
- В физических слотах 0..6 по смещению 3552 найдены корректные константы
  OBN1-r1 и seq 2089..2095, epoch=7, clean=1, raw, nint=1, N=4096,
  counter Hz=62500000. В слоте 7 по этому смещению валидного header нет.
  Простой поворот всего кольца или восстановление границ записей не доказаны.
- Разности first_tick для этих последовательных header fragments:
  8333, 8333, 8333, 8334, 8333, 8333. Ожидается 4096×62500000/30720000 =
  8333.333… ticks/window. Это согласованная аппаратная временная метка; она не
  исправляет нарушенное размещение payload и не подтверждает целые наблюдения.

Ошибка соответствует границе `native_observation::validate/drain`: контракт
требует header в словах 0..31 и нулевые верхние halfwords после слова 31.
Реальный слот этому не соответствует. ARM decoder/порядок bins не смягчались;
сдвиг на 544 слова в ARM не вводился, неправильные данные hardware GOBS1 не объявлялись.

Причина сдвига **не установлена**. Проверять конкретные DMA prefetch/bank/read-address
условия и переход full manager на i5 по сохранённым данным. Само наличие AW,
busy=0, старое описание поворота спектра или успешные цифровые тесты причиной
не объявляются. Новые HDL/ABI/DDR/reset изменения требуют обсуждения с Глебом.
После опыта Глеб напомнил об известном смещении спектра при RAM-загрузке и
запаздывании счёта отсчётов. Наблюдаемые 544 слова согласуются с этой подсказкой;
это важный контекст для i5, но механизм и безопасное исправление здесь не доказаны.

## Измерения и локальные доказательства

| Показатель | Фактический результат |
|---|---|
| План / фактическое acquisition | 90 s / остановка на первой серии, около 7 ms после READY |
| Produced snapshot / selected / recorded / native lost | 2117 / 8 / 8 / 0 |
| Пригодные GOBS1 наблюдения fast / slow | 0 / 0; оба timeout 60 s |
| Header fragments, найденные в неверной позиции | 7; seq 2089..2095, epoch 7, nint1 |
| Tick spacing / nominal | 8333–8334 / 8333.333… (62.5 MHz domain, Fs 30.72 Msps) |
| Process RSS / CPU | 6356–7316 KiB, накопленные CPU 0.94 s; 48 samples за ~116 s, преимущественно ожидание UIO после abort; не замер работающего тракта |
| Traffic / slow-client queue loss / bounded-memory stress | Не измерены: нет принятого native потока |
| Новые kernel DMA/IIO/AXI ошибки относительно post-restore baseline | Не найдены; native status fault/reject=0 |

Все пути ниже относительно `C:/proj/pluto_ag/evidence/obs-003-hw/`:

- `uart.log`, `uart-events.log` — UART, команды и загрузки; `baseline.json`,
  `baseline-dmesg.txt`, `preflight.txt`, `rf-readback.txt` — исходная точка.
- `ci-run.json`, локальный run artifact manifest — версии исправленной ARM пары.
- `native.json`, `agent.log`, `monitor.jsonl`, `native-stop.txt`, `before-power.txt` —
  реальный source lifetime, снятие validity, UIO wait, завершение и marker.
- **`native-slot0-devmem.bin`**, 32768 bytes, SHA256
  `df8619c52e2500ac5027f845cb9685869c3eab75a1c2b50fe8d02c7ba4a19495`;
  `completed-devmem.txt`, `native-independent.json`, `decode_completed.py`.
- `shifted-headers.txt`, `shifted-headers.json`, `decode_shifted.py` — реальные
  header fragments по неверному смещению; не исправленные ABI records.
- `fast.json`, `slow.json`, `fast-reader.log`, `slow-reader.log` — два отказа чтения;
  итогового GOBS capture нет. `guard-refusal/` — предыдущий отказ до доступа к DMA.
- `native-dmesg.txt`, `final-experimental-dmesg.txt`, `dmesg-comparison.json` —
  отсутствие новых сообщений DMA/IIO/AXI. Синтетические fixtures сюда не подмешивались.

## Аппаратные проверки

| Проверка | Результат |
|---|---|
| Физический UART и один владелец | PASS |
| Проверенные байты PL/ELF/скрипта в RAM | PASS |
| UART load → status → restore / OBS_READY | PASS |
| Guard и startup OBN1-r1 на реальной паре | PASS после адресной правки ARM |
| Native DMA → ARM → инструментальный GOBS1 reader | FAIL: неверное размещение header, принятых GOBS наблюдений 0 |
| Размещение native record / все 4096 bins / нормировка до GOBS | FAIL: исходная запись нарушает ABI; побиновую приёмку выполнить нельзя |
| Диагностическое чтение seq/nint/epoch/ticks | PASS только для 7 смещённых header fragments, не для целых наблюдений |
| Диагностическая смена контекста при неизменном RF | NOT RUN |
| Медленный клиент / потери / память / CPU | NOT RUN |
| Короткий real8 smoke на OBN1 | NOT RUN: приёмка пары остановлена на конкретном DMA/ABI блокере; legacy consumer не запускался |
| Финальный возврат рабочей системы и свежий спектр | PASS после подтверждённого физического питания |
| Штатный real8 после питания | PASS, 177 blocks / 3.03 s, lost_blocks=0; качество видео не принималось |

Dirty/epoch0, diagnostic token transition, slow-client pressure и нормальный
native CPU/RSS/traffic не проверены на железе. Их прежнее цифровое покрытие
сохраняется в своих границах. Минимальный RF settling, fastlock throughput и RF
dBm calibration не входят в приёмку. IndexError 66/64 в коротком штатном smoke не
наблюдался; причина прежнего бага остаётся открытой, clamp/drop не добавлялись.

## Физический возврат и состояние платы

Глеб подтвердил выключение/включение питания. Через API/SSH наблюдалась новая
загрузка с boot_id **`1e70da61-e5b9-4eaa-aaa2-56cb56a84bb9`** и установленной
Maia5162a79 / tezuka-5162. ELF `/usr/bin/maia-httpd` имеет исходный SHA256;
S60maia-httpd, api_controller и pl_video_ctl работают. `0x7c46003c=0xFD5F0000`
подтверждает PL-5b, FM mode=0. Штатный ELF на OBN1 до питания не запускался.

По просьбе Глеба контроль ограничен коротким smoke:

- `restored-spectrum.json/.npz`: 22 свежих /waterfall кадра за 3.076 s,
  7.3251 fps; все 4 сохранённых payload без tag различны.
- Короткий экземпляр существующего `host_smoke.py` (только интервалы 30→3 s,
  video settle 3→1 s, окно 15→3 s): source maia connected, rows 59→125,
  60/60 уникальных WS сообщений; **177 real8 блоков за 3.03 s**, loss=0,
  около 3.83214 M CS16 words/s, mode/format consistency=true, overrun=false.
  Демодулятор search, картинки/lock нет, clip counter был saturated; этим
  проверено поступление транспорта, качество RF/видео не заявляется.
- Хост штатно остановлен (stop_rc=0). Временные RF/spectrometer/routing
  настройки возвращены; Fs=30.72 Msps, LO=2.4 GHz, BW=18 MHz, SlowAttack,
  spectrometer AD9361/Average/nint1023. TX_LO_powerdown=1, attenuation=-89.75 dB.
- boot MTD0, env MTD1 и installed image MTD3 SHA256 совпали с исходными;
  хеши init-скриптов и существующих файлов настроек /mnt/jffs2 совпали.
  Новых постоянных boot/autostart изменений не внесено.

Доказательства: `restored.json`, `restored-dmesg.txt`, `restored-check.txt`,
`restored-short-host.json`, `restored-short-*.log`, `stock_host_smoke.py`.
Рабочий хост до теста не был запущен; smoke использовал отдельный порт 18001
и state внутри этого каталога, затем завершил только свой экземпляр.

Физический COM5 после питания исчез из Windows port inventory; остался COM8
(USB console). Поэтому UART boot transcript финального цикла не получен;
его реальность подтверждены сообщением Глеба, новым boot_id и проверкой рабочей
системы. Предыдущий UART transcript на Pentium сохранён. Владелец COM5 завершён,
новых команд/фонового захвата нет. Перед следующим RAM load восстановить
физическое UART-подключение и проверить фактический порт, COM8 не подставлять.

## Следующий шаг

Возврат PL-5b, штатный спектр/real8 и TX off подтверждены. Передать Глебу/i5
конкретный slot0 dump и shifted-headers.json: устранить
несоответствие расположения native записи согласованному ABI. Отдельно перед
следующим hardware lifetime сделать native error/shutdown диагностируемыми без
ожидания legacy UIO read. Повтор — после согласованной правки и штатного питания,
с новым capture UUID. DFU/flash, новый DT/ядро, слепое удаление marker, SDR-only
reset и ARM-компенсация смещения не являются установленными следующими действиями.
