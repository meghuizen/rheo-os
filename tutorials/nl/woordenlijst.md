# Woordenlijst

Alle moeilijke woorden uit dit boek, kort uitgelegd. Op alfabet.

- **Adres** - het nummer van een vakje in het geheugen. Met een adres zeg je
  tegen de processor *waar* iets staat.

- **ARM64 (AArch64)** - de instructieset van bijna alle telefoons en tablets, en
  van veel nieuwe laptops en servers. Netjes ontworpen en zuinig.

- **Assembler** - het programma dat assembly omzet in machinecode.

- **Assembly** - een leesbare vorm van machinecode. Eén regel is meestal één
  instructie.

- **BIOS** - de oude firmware van pc's. Start bij het aanzetten en laadt de
  bootsector.

- **Bit** - het kleinste stukje informatie: een 0 of een 1.

- **Bootloader** - het eerste programma dat draait bij het opstarten. Zijn taak
  is de kernel starten.

- **Bootsector** - bij x86: het eerste blokje van 512 bytes op een schijf, dat de
  BIOS laadt. Eindigt op de handtekening `0x55 0xAA`.

- **Byte** - acht bits samen. Eén byte kan een getal van 0 tot en met 255 zijn.

- **Cross-toolchain** - gereedschap dat op jouw computer (de host) draait, maar
  programma's maakt voor een andere processor (het target).

- **Driver** - code die met een apparaat praat (schijf, netwerk, scherm).

- **Emulator** - een programma dat een hele computer nadoet in software. Wij
  gebruiken QEMU.

- **Endianness** - de volgorde waarin de bytes van een groot getal in het
  geheugen staan. Onze drie ISA's zijn *little-endian* (kleinste byte eerst).

- **Firmware** - software die vast in een chip zit en klaar is zodra de stroom
  aangaat (BIOS, UEFI, OpenSBI).

- **Gebruikersruimte (user space)** - de wereld waar gewone programma's draaien.
  Ze mogen beperkt en vragen de rest aan de kernel.

- **Gebruikers-stand (user mode)** - de beperkte stand van de processor voor
  gewone programma's.

- **Geheugen (RAM)** - een lange rij vakjes waarin de computer getallen bewaart.
  Elk vakje heeft een adres.

- **Hex (hexadecimaal)** - een telstelsel met zestien cijfers (0-9 en a-f).
  Getallen met `0x` ervoor staan in hex, bijvoorbeeld `0x7c00`.

- **Host** - jouw eigen computer, waarop je bouwt.

- **Instructie** - één klein stapje dat de processor uitvoert.

- **Instructieset (ISA)** - de "taal" van een processor: welke instructies hij
  begrijpt en de regels erbij.

- **Interrupt** - een onderbreking: de hardware tikt de processor op de schouder
  als er iets is (toets, klok, netwerk). Ook de x86-BIOS-hulpjes (`int 0x10`)
  heten zo.

- **Kernel** - het hart van het besturingssysteem. Mag alles: bij alle hardware
  en al het geheugen.

- **Kernel-stand (supervisor / ring 0)** - de machtige stand van de processor
  waarin de kernel draait.

- **Label** - een naam voor een plek in de code, zoals `_start` of `volgende`.

- **Linker** - het programma dat objectbestanden samenvoegt en bepaalt op welk
  adres de code terechtkomt.

- **Little-endian** - zie *endianness*: de kleinste byte van een getal staat
  eerst in het geheugen.

- **Load** - een byte (of getal) uit het geheugen lezen.

- **Machinecode** - instructies als kale getallen; de enige taal die de processor
  echt begrijpt.

- **Memory-mapped I/O** - hardware aansturen door naar speciale adressen te
  schrijven of van ze te lezen (bijvoorbeeld de seriële poort).

- **Multi-core** - een computer met meerdere processoren (kernen) die tegelijk
  werken.

- **Paging** - het geheugen in blokjes (pagina's) verdelen en per programma
  bepalen welke blokjes het mag gebruiken.

- **Pollen** - steeds zelf controleren of er iets gebeurd is (het tegenovergestelde
  van wachten op een interrupt).

- **Processor (CPU)** - het onderdeel dat instructies uitvoert.

- **Proces** - een draaiend programma met zijn eigen geheugen.

- **Program counter** - het register dat aanwijst welke instructie nu aan de beurt
  is.

- **QEMU** - de emulator die wij gebruiken om de drie processoren na te doen.

- **Real mode** - de oude 16-bit stand waarin een x86 opstart.

- **Register** - een heel snel opslagplekje in de processor, voor één getal.

- **RISC-V** - een open, eenvoudige instructieset. Onze startplek in dit boek.

- **Scheduling (inplannen)** - programma's om de beurt de processor geven, heel
  snel achter elkaar, zodat ze lijken samen te draaien.

- **Seriële poort (UART)** - een simpel stukje hardware dat tekens één voor één
  verstuurt. Onze eerste manier om tekst te tonen.

- **Stack (stapel)** - een stukje geheugen dat functies gebruiken voor tijdelijke
  gegevens. Groeit naar beneden.

- **Stack pointer** - het register dat aanwijst waar de bovenkant van de stack is
  (bij RISC-V heet dat `sp`).

- **Store** - een byte (of getal) naar het geheugen schrijven.

- **Syscall (systeemaanroep)** - een verzoek van een gewoon programma aan de
  kernel, bijvoorbeeld "open dit bestand".

- **Target (doel)** - de processor waarvoor je een programma maakt.

- **UEFI** - de moderne opvolger van de BIOS.

- **x86-64 (amd64)** - de instructieset van de meeste laptops en desktops. Oud en
  daardoor ingewikkeld, met een bijzondere opstartmanier.

Terug naar de [inhoudsopgave](README.md).
