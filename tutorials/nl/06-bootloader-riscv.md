# Hoofdstuk 6 - Bootloader voor RISC-V (de makkelijkste om te beginnen)

Dit is het hoofdstuk waar je naartoe hebt gewerkt. We schrijven onze eerste
bootloader. Aan het eind zie je met je eigen ogen "Hallo van RISC-V!" op je
scherm, gemaakt door code die jij helemaal zelf hebt geschreven, zonder
besturingssysteem eronder.

We beginnen met RISC-V omdat het de eenvoudigste ISA is. De andere twee volgen in
hoofdstuk 7 en 8, en je zult zien dat het idee precies hetzelfde blijft.

## Wat gaan we maken?

Een programma dat:

1. het adres van de seriële poort onthoudt (`0x10000000` op QEMU virt);
2. letter voor letter door onze tekst loopt;
3. elke letter naar de seriële poort schrijft;
4. daarna blijft hangen (er is niks meer te doen).

## Stap 1 - Het assembly-bestand

Maak in je werkmap een bestand `boot.s` met deze inhoud. Typ het zelf over; de
`#`-regels zijn commentaar (uitleg voor mensen, de computer negeert ze).

```asm
# boot.s - een minimale RISC-V bootloader voor QEMU 'virt'

.section .text          # hier begint de code
.globl _start           # maak _start zichtbaar voor de linker
_start:
    li      t0, 0x10000000   # t0 = adres van de seriele poort (UART)
    la      t1, boodschap    # t1 = adres van onze tekst

volgende:
    lb      t2, 0(t1)        # laad 1 byte (1 letter) uit de tekst in t2
    beqz    t2, klaar        # is die byte 0? dan zijn we aan het eind
    sb      t2, 0(t0)        # schrijf de letter naar de UART -> op scherm
    addi    t1, t1, 1        # ga naar de volgende letter
    j       volgende         # herhaal

klaar:
    j       klaar            # blijf hier hangen (oneindige lus)

.section .rodata        # hier komt data die niet verandert
boodschap:
    .string "Hallo van RISC-V!\n"
```

### Wat betekent elke regel?

- `.section .text` - "wat hierna komt, is code." De CPU voert dit uit.
- `.globl _start` - `_start` is de naam van ons startpunt. De `.globl` zorgt dat
  de linker (zo dadelijk) dit punt kan vinden.
- `_start:` - een **label**: een naam voor een plek in de code. `_start` is waar
  het programma begint.
- `li t0, 0x10000000` - **li** staat voor *load immediate*: zet dit getal direct
  in register `t0`. We onthouden hier het adres van de seriële poort.
- `la t1, boodschap` - **la** = *load address*: zet het adres van `boodschap` in
  `t1`. Nu wijst `t1` naar de eerste letter van onze tekst.
- `lb t2, 0(t1)` - **lb** = *load byte*: haal de byte op het adres in `t1` op en
  zet die in `t2`. De `0(t1)` betekent "het adres in t1, plus 0".
- `beqz t2, klaar` - **beqz** = *branch if equal zero*: als `t2` gelijk is aan 0,
  spring dan naar het label `klaar`. Onze tekst eindigt op een onzichtbare 0
  (daar zorgt `.string` voor), dus zo weten we wanneer we klaar zijn.
- `sb t2, 0(t0)` - **sb** = *store byte*: schrijf de byte in `t2` naar het adres
  in `t0`. En `t0` is de seriële poort. Dit is het moment dat de letter op je
  scherm komt.
- `addi t1, t1, 1` - **addi** = *add immediate*: tel 1 op bij `t1`. Nu wijst `t1`
  naar de volgende letter.
- `j volgende` - **j** = *jump*: spring terug naar `volgende`. Zo ontstaat een
  lus die alle letters afgaat.
- `klaar: j klaar` - een lus die naar zichzelf springt: het programma "hangt"
  hier expres. Er is immers niks meer te doen, en stoppen zonder OS bestaat niet
  echt.
- `.section .rodata` - "wat hierna komt, is data die niet verandert" (rodata =
  *read-only data*).
- `.string "..."` - onze tekst. De assembler zet er automatisch een 0 achter,
  precies waar `beqz` op controleert.

## Stap 2 - Het linker-script

De **linker** bepaalt op welk adres onze code terechtkomt. Voor QEMU virt (RISC-V)
moet onze code op adres `0x80000000` staan, want daar springt QEMU naartoe.

Maak een bestand `linker.ld`:

```ld
ENTRY(_start)

SECTIONS
{
  . = 0x80000000;       /* plaats alles vanaf dit adres */
  .text   : { *(.text*)   }
  .rodata : { *(.rodata*) }
  .data   : { *(.data*)   }
  .bss    : { *(.bss*)    }
}
```

- `ENTRY(_start)` - vertel de linker dat het programma begint bij `_start`.
- `. = 0x80000000;` - de punt `.` is "de huidige plek". We zetten hem op
  `0x80000000`. Alles wat volgt, komt vanaf dat adres.
- De regels eronder plaatsen de code (`.text`) en de data netjes op volgorde.

## Stap 3 - Bouwen

Nu vertalen we `boot.s` naar een programma. Dat gaat in twee stappen: eerst
**assembleren** (assembly -> objectbestand), dan **linken** (objectbestand ->
uitvoerbaar programma op het juiste adres).

```console
$ riscv64-linux-gnu-as -march=rv64g boot.s -o boot.o
$ riscv64-linux-gnu-ld -T linker.ld boot.o -o boot.elf
```

- `riscv64-linux-gnu-as` - de assembler voor RISC-V. `-march=rv64g` zegt: gebruik
  de standaard 64-bit RISC-V instructies.
- `riscv64-linux-gnu-ld` - de linker voor RISC-V. `-T linker.ld` gebruikt ons
  script. Het resultaat is `boot.elf`.

Krijg je "command not found"? Dan mist de cross-toolchain nog. Ga terug naar
[hoofdstuk 4](04-je-gereedschap.md).

## Stap 4 - Draaien

Start QEMU en laat onze bootloader draaien:

```console
$ qemu-system-riscv64 -machine virt -nographic -bios none -kernel boot.elf
```

Wat betekenen de opties?

- `-machine virt` - doe een standaard, virtueel RISC-V-bordje na.
- `-nographic` - geen apart venster; gebruik gewoon deze terminal voor de
  seriële poort. Precies wat we willen.
- `-bios none` - geen firmware ervoor; onze code is het eerste dat draait.
- `-kernel boot.elf` - laad ons programma.

Je zou nu moeten zien:

```text
Hallo van RISC-V!
```

Daarna lijkt QEMU te "hangen" - dat klopt, dat is onze `klaar: j klaar`-lus.

## Stap 5 - QEMU afsluiten

Omdat we `-nographic` gebruiken, sluit je QEMU af met een toetsencombinatie:
druk op **Ctrl-A**, laat los, en druk dan op **X**.

## Het overzicht in een Makefile (handig)

Steeds die commando's typen is vervelend. Maak een `Makefile` (let op: gebruik
**tabs**, geen spaties, voor de ingesprongen regels):

```makefile
boot.elf: boot.s linker.ld
	riscv64-linux-gnu-as -march=rv64g boot.s -o boot.o
	riscv64-linux-gnu-ld -T linker.ld boot.o -o boot.elf

run: boot.elf
	qemu-system-riscv64 -machine virt -nographic -bios none -kernel boot.elf

clean:
	rm -f boot.o boot.elf
```

Nu kun je gewoon typen:

```console
$ make run
```

## Het ging mis - wat nu?

- **Niks op het scherm.** Controleer het adres `0x10000000` en dat je `-machine
  virt` gebruikt. Controleer of `.string` echt in je code staat.
- **"command not found".** De toolchain of QEMU mist. Zie hoofdstuk 4.
- **QEMU sluit meteen af of geeft een foutmelding.** Controleer je `linker.ld`
  (staat het adres op `0x80000000`?) en of `_start` goed gespeld is.
- **Rare tekens.** Controleer je tekst tussen de aanhalingstekens.

Fouten maken hoort erbij. Lees de foutmelding rustig; er staat vaak een hint in.

## Samenvatting

- Je schreef een echte bootloader in RISC-V-assembly.
- Je gebruikte **li/la/lb/sb/addi/j/beqz**: laden, adres pakken, byte lezen, byte
  schrijven, optellen, springen, en springen-als-nul.
- Het **linker-script** plaatste je code op `0x80000000`, waar QEMU naartoe
  springt.
- Je schreef letters naar de seriële poort op `0x10000000` en zag ze verschijnen.

Dit is echt een mijlpaal. Jouw code draaide als eerste op de (nagedane) machine.

## Oefening

1. Verander de tekst naar je eigen naam. Bouw en draai opnieuw.
2. Laat twee regels zien in plaats van één. (Tip: gebruik `\n` in de tekst, of
   maak een tweede `.string`.)
3. Leg in je eigen woorden uit waarom we op `0x80000000` moeten zitten.
4. Wat gebeurt er als je de `beqz`-regel weghaalt? Bedenk het eerst, probeer het
   dan.

Door naar [hoofdstuk 7](07-bootloader-arm64.md): dezelfde bootloader, maar nu
voor ARM64. Je zult zien hoeveel hetzelfde blijft.
