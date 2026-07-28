# Hoofdstuk 9 - Van bootloader naar kernel

Onze bootloaders uit hoofdstuk 6, 7 en 8 doen één ding: tekst tonen. Dat is een
prachtig begin, maar een echt besturingssysteem is veel te groot om helemaal in
assembly te schrijven. In dit hoofdstuk leer je de brug: hoe je vanuit een klein
stukje assembly overspringt naar een **kernel** die je in een makkelijkere taal
(zoals C of Rust) schrijft.

We doen het voorbeeld in RISC-V, omdat je die het beste kent. Het idee werkt op
alle drie de ISA's.

## Waarom niet alles in assembly?

Assembly is dicht bij de processor, maar traag om in te schrijven en makkelijk
fout te doen. Een echte kernel heeft duizenden regels code nodig. Dat wil je in
een taal met functies, variabelen en `if`-statements schrijven, niet met kale
instructies.

Daarom is de taakverdeling bijna altijd:

- **Een heel klein stukje assembly** doet de dingen die je in een gewone taal
  niet kunt: het allereerste opstarten, en het klaarzetten van de **stack**.
- **De rest** schrijf je in C of Rust, in een gewone functie zoals `kernel_main`.

## Wat is een stack, en waarom eerst?

Een **stack** (Nederlands: stapel) is een stukje geheugen dat functies gebruiken
om tijdelijke dingen in op te slaan: waar ze naartoe moeten terugkeren, en hun
lokale variabelen. Elke keer dat je een functie aanroept, komt er iets bovenop de
stapel; als de functie klaar is, gaat het er weer af.

Belangrijk: code in C of Rust *gaat ervan uit* dat er al een stack is. Een
speciaal register, de **stack pointer** (bij RISC-V heet dat `sp`), moet naar een
stukje vrij geheugen wijzen. Als je dat niet klaarzet en je roept toch een
functie aan, crasht alles.

Daarom is de eerste taak van onze assembly: **de stack pointer instellen**. Pas
daarna mogen we naar C of Rust springen.

## Stap 1 - De opstart-assembly

Maak `start.s`. Dit is bijna hetzelfde als in hoofdstuk 6, maar in plaats van
zelf tekst te tonen, zet het de stack klaar en roept het `kernel_main` aan:

```asm
# start.s - zet de stack klaar en spring naar de kernel (RISC-V)

.section .text
.globl _start
_start:
    la      sp, stack_top     # zet de stack pointer op de top van onze stack
    call    kernel_main       # spring naar de C-functie kernel_main

hang:
    j       hang              # mocht kernel_main ooit terugkeren: blijf hangen

.section .bss
    .align 4
stack_bottom:
    .skip 4096                # reserveer 4096 bytes (4 KB) voor de stack
stack_top:
```

Uitleg van het nieuwe:

- `.section .bss` - de `.bss` is geheugen dat bij de start op 0 staat. Perfect om
  een stack in te reserveren; we hoeven er niks in te zetten, alleen ruimte.
- `.skip 4096` - reserveer 4096 lege bytes. Dat is onze stack.
- `stack_top:` - een label helemaal aan het **einde** van die ruimte. Waarom het
  einde? Omdat een stack "naar beneden groeit": de stack pointer begint bovenaan
  en gaat omlaag naarmate je functies aanroept. Dat is een afspraak van de ISA.
- `la sp, stack_top` - zet `sp` op die bovenkant. Nu is er een geldige stack.
- `call kernel_main` - spring naar onze kernelfunctie. **call** onthoudt ook waar
  we vandaan kwamen, zodat de functie later kan terugkeren.

## Stap 2 - De kernel in C

Maak `kernel.c`. Dit is een gewone C-functie - veel prettiger dan assembly:

```c
/* kernel.c - onze eerste kernel in C */

/* Het adres van de seriele poort (UART) op QEMU virt, RISC-V. */
#define UART 0x10000000

/* Schrijf een letter naar de seriele poort. */
void putc(char c) {
    volatile char *uart = (volatile char *)UART;
    *uart = c;                 /* schrijf de byte -> op scherm */
}

/* Schrijf een hele tekst (die eindigt op 0). */
void puts(const char *s) {
    while (*s) {               /* zolang de letter niet 0 is */
        putc(*s);
        s++;                   /* volgende letter */
    }
}

/* Dit is waar start.s naartoe springt. */
void kernel_main(void) {
    puts("Hallo van de kernel, geschreven in C!\n");
    while (1) { }              /* blijf hangen */
}
```

Zie je hoe veel leesbaarder dit is? Een `while`-lus, een functie `putc`, een
functie `puts`. Precies dezelfde tekst-tonen-lus als in assembly, maar nu in een
taal die je makkelijk kunt uitbreiden.

Kleine uitleg:

- `volatile` - dit sleutelwoord zegt tegen de vertaler: "dit adres is echte
  hardware, wees hier niet slim, schrijf precies wat ik zeg." Zonder `volatile`
  zou de vertaler onze schrijfactie soms "wegoptimaliseren". Bij hardware wil je
  dat nooit.
- De rest is gewone C.

## Stap 3 - Linker-script

Hetzelfde idee als in hoofdstuk 6, maar nu voegen we ook een `.bss` toe (voor de
stack). Maak `linker.ld`:

```ld
ENTRY(_start)

SECTIONS
{
  . = 0x80000000;
  .text   : { *(.text*)   }
  .rodata : { *(.rodata*) }
  .data   : { *(.data*)   }
  .bss    : { *(.bss*)    }
}
```

## Stap 4 - Bouwen

Nu bouwen we twee bestanden (de assembly en de C) en linken ze samen:

```console
$ riscv64-linux-gnu-gcc -c -march=rv64g -mabi=lp64 -ffreestanding start.s -o start.o
$ riscv64-linux-gnu-gcc -c -march=rv64g -mabi=lp64 -ffreestanding kernel.c -o kernel.o
$ riscv64-linux-gnu-ld -T linker.ld start.o kernel.o -o kernel.elf
```

Nieuwe dingen:

- We gebruiken nu `riscv64-linux-gnu-gcc` (de C-vertaler) ook voor de assembly;
  dat is handig omdat gcc alles begrijpt.
- `-ffreestanding` - heel belangrijk: "er is geen besturingssysteem, geen
  standaardbibliotheek." Precies onze situatie. Zonder dit denkt de vertaler dat
  er een OS onder zit.
- `-mabi=lp64` - de afspraak over hoe getallen in registers passen (64-bit).

## Stap 5 - Draaien

```console
$ qemu-system-riscv64 -machine virt -nographic -bios none -kernel kernel.elf
```

Je zou moeten zien:

```text
Hallo van de kernel, geschreven in C!
```

Gefeliciteerd: je hebt nu een kernel die in C is geschreven, gestart door je
eigen opstart-assembly. Vanaf hier kun je de kernel uitbreiden zonder nog veel
assembly te hoeven schrijven.

## De grote structuur die je nu hebt

```text
_start (assembly)         <- zet de stack klaar
   |
   v
kernel_main (C)           <- hier bouw je de rest van je OS
   |
   +-- putc / puts        <- praten met de hardware
   +-- (later) geheugenbeheer, processen, drivers, ...
```

Bijna elk echt besturingssysteem heeft deze vorm: een dun laagje assembly per
ISA, en daarboven een grote kernel in een gewone taal. Het rheo-os-project in dit
repository doet precies dit: kleine assembly-stukjes in `kernel/arch/`, en de
rest in Rust. Rust wordt vaak gekozen omdat het je beschermt tegen veelgemaakte
geheugenfouten - handig als één fout de hele computer kan laten crashen.

## En op ARM64 en x86?

Hetzelfde idee:

- **ARM64**: je assembly zet `sp` en springt naar `kernel_main`. Het UART-adres in
  je C-code wordt `0x09000000`.
- **x86**: ingewikkelder, want je moet eerst van 16-bit naar 32-bit naar 64-bit
  overschakelen voordat gewone C-code fijn draait. Daarom laten veel mensen op
  x86 de firmware (UEFI) of een bestaande bootloader (zoals GRUB of Limine) dat
  overschakelen doen, en begint hun C-code pas daarna. Dat is een mooi
  vervolgproject.

## Samenvatting

- Een echte kernel schrijf je niet in assembly, maar in C of Rust.
- Een klein stukje assembly is nog wel nodig: het zet de **stack** klaar (de
  stack pointer `sp`) en springt dan naar `kernel_main`.
- Met `-ffreestanding` bouw je code die geen besturingssysteem onder zich nodig
  heeft.
- Deze structuur - dun assembly-laagje per ISA, grote kernel in een gewone taal -
  gebruikt bijna elk OS, ook rheo-os.

## Oefening

1. Voeg een functie `puts_regel(const char *s)` toe die de tekst toont en er zelf
   een `\n` achteraan zet.
2. Laat `kernel_main` drie verschillende regels tonen.
3. Wat gebeurt er als je in `start.s` de regel `la sp, stack_top` weghaalt?
   Bedenk het eerst, test het dan. (Hint: functies hebben een stack nodig.)
4. Maak de ARM64-variant: pas het UART-adres in `kernel.c` aan en schrijf een
   `start.s` die `sp` zet en `kernel_main` aanroept.

Door naar [hoofdstuk 10](10-volgende-stappen.md): de weg naar een echt
besturingssysteem.
