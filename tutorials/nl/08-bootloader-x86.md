# Hoofdstuk 8 - Bootloader voor x86 (de klassieke)

x86 is de oudste van onze drie ISA's, en dat merk je aan het opstarten. Op RISC-V
en ARM64 sprong QEMU netjes naar ons programma. Op x86 is het opstarten een stukje
ouderwetser - en juist daarom heel leerzaam. Dit is de manier waarop pc's al
tientallen jaren opstarten.

## De bootsector: precies 512 bytes

Bij een klassieke x86-pc werkt het zo:

1. De **BIOS** (de firmware) start.
2. De BIOS zoekt een schijf en leest daarvan het **allereerste blokje** van 512
   bytes. Dat blokje heet de **bootsector**.
3. De BIOS zet die 512 bytes in het geheugen op adres `0x7C00`.
4. De BIOS controleert of de laatste twee bytes `0x55` en `0xAA` zijn. Dat is de
   **boothandtekening** (Engels: *boot signature*). Zonder die handtekening denkt
   de BIOS: "dit is geen opstartbare schijf" en doet niks.
5. Klopt de handtekening? Dan springt de BIOS naar `0x7C00` en draait onze code.

Dus onze x86-bootloader moet:

- passen in **512 bytes**;
- **eindigen** op de twee bytes `0x55 0xAA`;
- **starten** alsof hij op adres `0x7C00` staat.

## 16-bit? Ja, echt

Nog iets ouderwets: een x86 begint in een oude stand die **real mode** heet, en
die is **16-bit**. Al is je processor 64-bit, bij het opstarten doet hij eerst
alsof hij een chip uit 1980 is. Dat is puur voor compatibiliteit met heel oude
software. Later "schakel" je zelf over naar 32-bit en dan 64-bit, maar dat is een
gevorderd onderwerp. Voor onze eerste bootloader blijven we lekker in 16-bit.

Het fijne aan 16-bit real mode: de BIOS biedt kant-en-klare hulpjes aan. Eén
ervan tovert een teken op het scherm. Die gebruiken we, zodat we niet zelf met
UART-adressen hoeven te werken zoals bij RISC-V en ARM.

## De BIOS om hulp vragen: de interrupt

In 16-bit real mode roep je een BIOS-hulpje aan met de instructie `int`
(*interrupt*). Je zet eerst in de registers *wat* je wilt, en dan zeg je `int
0x10` ("teken-en-scherm-dienst"). Concreet:

- `ah = 0x0e` betekent: "toon het teken in `al` op het scherm."
- `al` = het teken zelf.
- daarna `int 0x10`.

Dit is een ander soort "syscall": geen besturingssysteem, maar de BIOS die een
dienst aanbiedt.

## Stap 1 - Het assembly-bestand (NASM)

Voor x86 gebruiken we **NASM**. Maak `boot.asm`:

```asm
; boot.asm - een klassieke x86-bootsector voor QEMU

BITS 16                 ; we draaien in 16-bit real mode
org 0x7c00              ; de BIOS laadt ons op adres 0x7C00

start:
    mov si, boodschap   ; si wijst naar het begin van onze tekst

print:
    lodsb               ; laad de byte waar si naar wijst in 'al', si +1
    or  al, al          ; is al gelijk aan 0? (einde van de tekst)
    jz  done            ; ja -> spring naar done
    mov ah, 0x0e        ; BIOS-functie: 'toon teken'
    int 0x10            ; roep de BIOS aan -> teken verschijnt
    jmp print           ; herhaal voor het volgende teken

done:
    jmp done            ; blijf hier hangen

boodschap: db "Hallo van x86!", 0

times 510-($-$$) db 0   ; vul aan met nullen tot we op 510 bytes zitten
dw 0xaa55               ; de laatste 2 bytes: de boothandtekening
```

### Wat betekent elke regel?

- `; ...` - op x86 met NASM is `;` het teken voor commentaar.
- `BITS 16` - vertel NASM dat we 16-bit code maken.
- `org 0x7c00` - "doe alsof deze code op adres `0x7C00` staat." Dat moet, want
  daar zet de BIOS ons neer.
- `mov si, boodschap` - **mov** = zet iets ergens neer. Hier: zet het adres van
  `boodschap` in register `si`. `si` is een register dat vaak gebruikt wordt om
  "door iets heen te lopen".
- `lodsb` - laad de byte waar `si` naar wijst in `al`, en verhoog `si` met 1.
  Precies zoals `ldrb w2, [x1], #1` bij ARM64: laden én ophogen in één keer.
- `or al, al` - een truc om te testen of `al` nul is. (Iets met zichzelf "or-en"
  verandert de waarde niet, maar zet wel een vlaggetje als het nul is.)
- `jz done` - **jz** = *jump if zero*: als de vorige test "nul" opleverde, spring
  naar `done`. Het x86-broertje van `beqz` (RISC-V) en `cbz` (ARM64).
- `mov ah, 0x0e` en `int 0x10` - vraag de BIOS om het teken in `al` te tonen.
- `jmp print` - **jmp** = spring. Herhaal de lus.
- `done: jmp done` - blijf hangen; er is niks meer te doen.
- `db "Hallo van x86!", 0` - **db** = *define byte(s)*: onze tekst, met een 0
  erachter als eindteken (net als `.string` en `.asciz`).
- `times 510-($-$$) db 0` - vul de rest op met nullen tot we op 510 bytes zitten.
  `$-$$` is "hoeveel bytes hebben we tot nu toe"; we vullen aan tot 510.
- `dw 0xaa55` - **dw** = *define word* (2 bytes): de boothandtekening. Let op:
  x86 is little-endian, dus dit staat in het bestand als `0x55` gevolgd door
  `0xAA` - precies wat de BIOS wil zien.

512 = 510 opvulling + 2 handtekening. Klopt precies.

## Stap 2 - Bouwen

Bij x86 is er maar één stap, want we maken meteen een kaal binair bestand (geen
linker nodig):

```console
$ nasm -f bin boot.asm -o boot.img
```

- `-f bin` - maak een "plat" binair bestand, geen ELF. Een bootsector is puur 512
  losse bytes.
- `boot.img` - het resultaat. Dit stellen we straks voor als een schijf.

Controleer dat het echt 512 bytes is:

```console
$ ls -l boot.img
```

Er hoort `512` in de regel te staan.

## Stap 3 - Draaien

```console
$ qemu-system-x86_64 -drive format=raw,file=boot.img
```

- `-drive format=raw,file=boot.img` - hang `boot.img` in de machine als een
  simpele schijf. QEMU laat de BIOS ervan opstarten, precies zoals bij een echte
  pc.

Nu verschijnt er een **QEMU-venster** met daarin:

```text
Hallo van x86!
```

Let op: hier gebruiken we **geen** `-nographic`, want de BIOS tekent op het
beeldscherm (via `int 0x10`), niet op de seriële poort. Daarom zie je een echt
venster. Sluit het venster gewoon om te stoppen.

## Waarom is x86 zo anders?

Omdat x86 heel oud is en altijd oude software wil blijven draaien. Dat betekent:

- opstarten in 16-bit real mode;
- een bootsector van precies 512 bytes met een handtekening;
- de BIOS als hulpje voor het scherm.

Op RISC-V en ARM64 is dit allemaal moderner en simpeler geregeld. Nu snap je uit
eigen ervaring waarom we met RISC-V begonnen: minder ouderwetse regels om te
onthouden.

En toch: kijk naar de lus. `lodsb`, testen op nul, teken tonen, herhalen. Het is
hetzelfde idee als in hoofdstuk 6 en 7. Alleen de verpakking is anders.

## Het ging mis - wat nu?

- **"Boot failed" of niks.** Controleer dat je bestand precies 512 bytes is en
  dat de laatste regel `dw 0xaa55` er staat. Zonder handtekening start de BIOS
  niet.
- **Rare tekens of half beeld.** Controleer je tekst en de `int 0x10`-regel.
- **Geen venster.** Draai zonder `-nographic` (die had je hier niet moeten
  gebruiken). Op een server zonder scherm kun je `-display none -serial stdio`
  proberen, maar dan zie je de `int 0x10`-tekst niet; dat is een gevorderd
  onderwerp.

## Samenvatting

- x86 start ouderwets: **16-bit real mode**, een **bootsector van 512 bytes**,
  eindigend op de handtekening `0x55 0xAA`, geladen op `0x7C00`.
- We tonen tekst via de **BIOS** met `int 0x10` in plaats van rechtstreeks naar
  een UART te schrijven.
- Onder de motorkap is het idee identiek aan RISC-V en ARM64: loop door de tekst,
  toon elk teken, herhaal.

## Oefening

1. Haal de regel `dw 0xaa55` weg, bouw en draai. Wat gebeurt er? (En snap je nu
   waarom?)
2. Verander de tekst naar je eigen naam.
3. Zet de drie bootloaders (hoofdstuk 6, 7, 8) naast elkaar. Schrijf op: wat is
   het grootste verschil tussen x86 en de andere twee, en waardoor komt dat?

Door naar [hoofdstuk 9](09-van-bootloader-naar-kernel.md): hoe je van een
bootloader naar een echte kernel gaat.
