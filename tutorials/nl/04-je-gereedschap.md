# Hoofdstuk 4 - Je gereedschap installeren

Nu wordt het praktisch. In dit hoofdstuk installeren we alles wat je nodig hebt.
Het is een beetje saai werk, maar je doet het maar één keer.

Je hebt drie soorten gereedschap nodig:

1. **QEMU** - de emulator die de processoren nadoet.
2. Een **assembler** en **linker** - die je assembly omzetten naar een programma
   dat de (nagedane) computer kan draaien.
3. Een **teksteditor** - om je code te typen (VS Code, Vim, wat je fijn vindt).

We gebruiken de **GNU-toolchain** (de programma's `as`, `ld` en vrienden). Voor
x86 gebruiken we daarnaast **NASM**, omdat dat de klassieke, best uitgelegde
manier is om een boot-sector te maken.

> Let op: de commando's hieronder typ je in een **terminal**. Op Linux en macOS
> heet dat "Terminal". Op Windows raden we **WSL** aan (Windows Subsystem for
> Linux); dan heb je een Linux-terminal binnen Windows. Zoek "WSL installeren"
> op als je Windows gebruikt.

## Installeren op Ubuntu/Debian Linux (of WSL)

Kopieer dit regel voor regel in je terminal:

```console
$ sudo apt update
$ sudo apt install -y qemu-system-x86 qemu-system-arm qemu-system-misc
$ sudo apt install -y nasm
$ sudo apt install -y gcc-riscv64-linux-gnu gcc-aarch64-linux-gnu
$ sudo apt install -y build-essential
```

Uitleg per pakket:

- `qemu-system-x86`, `qemu-system-arm`, `qemu-system-misc` - de drie emulators
  (x86-64, ARM64 en RISC-V zitten hierin).
- `nasm` - de assembler voor onze x86-bootsector.
- `gcc-riscv64-linux-gnu` en `gcc-aarch64-linux-gnu` - dit zijn
  **cross-toolchains**. Een gewone `gcc` maakt programma's voor *jouw* processor.
  Een cross-toolchain maakt programma's voor een *andere* processor. Wij willen
  programma's voor RISC-V en ARM64 maken, ook al is je laptop misschien x86.
- `build-essential` - basisgereedschap zoals `make`.

## Installeren op macOS

Installeer eerst **Homebrew** (zoek "Homebrew installeren" op als je het nog
niet hebt), en dan:

```console
$ brew install qemu
$ brew install nasm
$ brew install riscv-gnu-toolchain
$ brew install aarch64-elf-gcc
```

De precieze namen kunnen per keer iets verschillen; als een naam niet werkt,
zoek dan met `brew search` (bijvoorbeeld `brew search riscv`).

## Controleren of het werkt

Test of QEMU er is:

```console
$ qemu-system-riscv64 --version
$ qemu-system-aarch64 --version
$ qemu-system-x86_64 --version
```

Elk commando hoort een versienummer te tonen. Zie je "command not found", dan is
dat pakket nog niet (goed) geïnstalleerd.

Test de assembler:

```console
$ nasm --version
```

## Een woord over "cross" en "target"

Je gaat vaak het woord **target** (Nederlands: doel) horen. Het target is de
processor waarvoor je een programma maakt. Als jij op een x86-laptop een
RISC-V-programma bouwt, dan is:

- de **host** (gastheer) = jouw x86-laptop;
- het **target** (doel) = RISC-V.

Een cross-toolchain is dus gereedschap dat op de host draait, maar voor een ander
target bouwt. Dat klinkt ingewikkeld, maar in de praktijk is het gewoon een
programma met `riscv64` of `aarch64` in de naam.

## Een werkmap maken

Maak een map waar je alles in gaat zetten:

```console
$ mkdir mijn-os
$ cd mijn-os
```

In de volgende hoofdstukken maken we hierin bestanden aan.

## Samenvatting

- We hebben **QEMU** (emulator), een **assembler/linker**, en een **editor**.
- Een **cross-toolchain** bouwt programma's voor een andere processor dan die van
  je eigen laptop.
- **Host** = jouw computer, **target** = de processor waarvoor je bouwt.
- Controleer met `--version` of alles is geïnstalleerd.

## Oefening

1. Draai de drie `qemu-system-... --version`-commando's. Werken ze alle drie?
2. Leg in je eigen woorden uit wat een cross-toolchain is.
3. Wat is bij jou de "host"? Een x86-laptop, een ARM-Mac, iets anders?

Klaar met installeren? Door naar [hoofdstuk 5](05-hoe-een-computer-opstart.md),
waar we leren hoe een computer eigenlijk opstart.
