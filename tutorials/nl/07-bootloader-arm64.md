# Hoofdstuk 7 - Bootloader voor ARM64

Nu doen we hetzelfde als in hoofdstuk 6, maar voor ARM64. Let goed op: het
*idee* is identiek. Alleen de registernamen, een paar instructienamen en het
adres van de seriële poort verschillen. Dit is precies de les uit hoofdstuk 3:
de ideeën blijven, de details veranderen.

## De verschillen met RISC-V, vooraf

| Ding | RISC-V | ARM64 |
|------|--------|-------|
| Registers | `t0`, `t1`, `t2` | `x0`, `x1`, `w2` |
| Seriële poort | `0x10000000` | `0x09000000` |
| Startadres | `0x80000000` | `0x40000000` |
| Byte laden + ophogen | twee instructies | één instructie (`ldrb ..., [x1], #1`) |

De seriële poort op ARM64 (QEMU virt) is een **PL011 UART**. Je stuurt een teken
door een byte te schrijven naar het adres `0x09000000`.

## Stap 1 - Het assembly-bestand

Maak `boot.s`:

```asm
// boot.s - een minimale ARM64 bootloader voor QEMU 'virt'

.section .text
.globl _start
_start:
    ldr     x0, =0x09000000    // x0 = adres van de PL011 UART
    ldr     x1, =boodschap     // x1 = adres van onze tekst

volgende:
    ldrb    w2, [x1], #1       // laad 1 byte in w2, en verhoog x1 met 1
    cbz     w2, klaar          // is die byte 0? dan zijn we klaar
    str     w2, [x0]           // schrijf de byte naar de UART -> op scherm
    b       volgende           // herhaal

klaar:
    b       klaar              // blijf hier hangen

.section .rodata
boodschap:
    .asciz "Hallo van ARM64!\n"
```

### Wat betekent elke regel?

- `//` - op ARM64 gebruiken we `//` voor commentaar (bij RISC-V was dat `#`).
- `ldr x0, =0x09000000` - **ldr** = *load register*. Met de `=` zeg je: zet dit
  getal (het UART-adres) in `x0`. Onder water zet de assembler dat getal ergens
  vlakbij neer en laadt het; jij hoeft je daar niet druk om te maken.
- `ldr x1, =boodschap` - zet het adres van onze tekst in `x1`.
- `ldrb w2, [x1], #1` - dit is een slimme instructie die twee dingen tegelijk
  doet: **ldrb** laadt één byte van het adres in `x1` in `w2`, en de `, #1`
  achteraan verhoogt `x1` daarna meteen met 1. Bij RISC-V had je hiervoor twee
  instructies nodig. (`w2` is de "kleine" 32-bit helft van register `x2`; voor
  één byte is dat prima.)
- `cbz w2, klaar` - **cbz** = *compare and branch if zero*: als `w2` nul is,
  spring naar `klaar`. Dit is het ARM-broertje van RISC-V's `beqz`.
- `str w2, [x0]` - **str** = *store register*: schrijf `w2` naar het adres in
  `x0` (de UART). Hier komt de letter op het scherm.
- `b volgende` - **b** = *branch* (spring). Het ARM-broertje van RISC-V's `j`.
- `.asciz "..."` - net als `.string` bij RISC-V: tekst met een 0 erachter.

Zie je? Regel voor regel doet dit exact hetzelfde als de RISC-V-versie.

## Stap 2 - Het linker-script

Op QEMU virt (ARM64) begint het RAM op `0x40000000`, en daar laadt QEMU onze
code. Maak `linker.ld`:

```ld
ENTRY(_start)

SECTIONS
{
  . = 0x40000000;
  .text   : { *(.text*)   }
  .rodata : { *(.rodata*) }
  .data   : { *(.data*)   }
  .bss    : { *(.bss*)    }
}
```

Alleen het adres (`0x40000000`) is anders dan bij RISC-V. De rest is identiek.

## Stap 3 - Bouwen

```console
$ aarch64-linux-gnu-as boot.s -o boot.o
$ aarch64-linux-gnu-ld -T linker.ld boot.o -o boot.elf
```

Dezelfde twee stappen als bij RISC-V (assembleren, dan linken), maar nu met de
ARM64-toolchain (`aarch64-linux-gnu-...`).

## Stap 4 - Draaien

```console
$ qemu-system-aarch64 -machine virt -cpu cortex-a53 -nographic -kernel boot.elf
```

- `-machine virt` - een standaard virtueel ARM64-bordje.
- `-cpu cortex-a53` - doe alsof we een Cortex-A53 processor zijn (een bekende
  ARM64-kern). QEMU wil voor ARM weten welk type CPU je nadoet.
- `-nographic` en `-kernel` - net als bij RISC-V.

Je zou moeten zien:

```text
Hallo van ARM64!
```

Afsluiten: **Ctrl-A**, dan **X** (net als in hoofdstuk 6).

## Stap 5 - Makefile

```makefile
boot.elf: boot.s linker.ld
	aarch64-linux-gnu-as boot.s -o boot.o
	aarch64-linux-gnu-ld -T linker.ld boot.o -o boot.elf

run: boot.elf
	qemu-system-aarch64 -machine virt -cpu cortex-a53 -nographic -kernel boot.elf

clean:
	rm -f boot.o boot.elf
```

## Waarom dit belangrijk is

Je hebt nu hetzelfde programma op twee compleet verschillende processoren
gedraaid. Kijk terug naar de tabel bovenaan: de verschillen zijn klein en te
overzien. De grote ideeën - laden, schrijven naar de seriële poort, een lus, een
label - zijn overal gelijk.

Dat is precies hoe echte OS-bouwers werken: ze schrijven het *idee* één keer op,
en per ISA vullen ze de kleine details in. (In dit project, rheo-os, staat om
die reden alle processor-specifieke code netjes apart in een map `arch/`, en de
rest is gedeeld.)

## Het ging mis - wat nu?

- **Niks op het scherm.** Controleer het adres `0x09000000` en dat je `-cpu
  cortex-a53` meegeeft.
- **Foutmelding bij het bouwen.** Controleer of je `aarch64-linux-gnu-as` hebt
  (hoofdstuk 4) en of je `//`-commentaar gebruikt, niet `#`.
- **QEMU doet niks.** Controleer het startadres `0x40000000` in `linker.ld`.

## Samenvatting

- Dezelfde bootloader, nu voor ARM64.
- Andere registernamen (`x0`, `x1`, `w2`), andere instructienamen (`ldr`, `ldrb`,
  `cbz`, `str`, `b`), ander UART-adres (`0x09000000`), ander startadres
  (`0x40000000`).
- Het idee bleef exact hetzelfde. Dat is de kern van draagbaar (portabel) werken.

## Oefening

1. Zet de RISC-V-versie (hoofdstuk 6) en de ARM64-versie naast elkaar. Streep de
   regels aan die *precies* hetzelfde doen.
2. Welk detail vond je het grootste verschil, en welk het kleinste?
3. Verander de tekst en draai opnieuw.

Door naar [hoofdstuk 8](08-bootloader-x86.md): de klassieke x86-bootsector. Die
is anders - en juist daarom leerzaam.
