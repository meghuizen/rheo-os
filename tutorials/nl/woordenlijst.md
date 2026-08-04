# Woordenlijst

Alle moeilijke woorden uit dit boek, kort uitgelegd. Op alfabet.

- **ABA-probleem** - een fout bij gelijktijdig programmeren: een waarde
  verandert van A naar B en weer terug naar A, waardoor een ander stuk code
  denkt dat er niets veranderd is, terwijl er tussenin van alles is gebeurd.

- **ACPI** - Advanced Configuration and Power Interface. Een standaard waarmee
  de firmware de kernel vertelt wat er in de computer zit: hoeveel processoren,
  hoeveel geheugen, welke apparaten, en hoe je ze aan- en uitzet.

- **Adres** - het nummer van een vakje in het geheugen. Met een adres zeg je
  tegen de processor *waar* iets staat.

- **Affiniteit (CPU-affiniteit)** - de voorkeur om een programma of thread op
  een bepaalde processorkern te laten draaien, bijvoorbeeld omdat de data daar
  al in de cache zit.

- **APIC** - Advanced Programmable Interrupt Controller. De interrupt-controller
  die in elke moderne x86-processor zit. Verdeelt interrupts over de kernen.
  Zie ook: PIC, GIC, PLIC.

- **ARM64 (AArch64)** - de instructieset van bijna alle telefoons en tablets, en
  van veel nieuwe laptops en servers. Netjes ontworpen en zuinig.

- **Assembler** - het programma dat assembly omzet in machinecode.

- **Assembly** - een leesbare vorm van machinecode. Een regel is meestal een
  instructie.

- **Async/await** - een manier van programmeren waarbij een taak zegt "ik wacht
  op iets" (await) zonder de processor vast te houden. Het systeem pakt
  ondertussen een andere taak op. Wordt gebruikt bij taken, strands en
  coroutines.

- **Atomaire operatie** - een bewerking die in een keer gebeurt, zonder dat een
  andere kern er tussenin kan komen. Bijvoorbeeld: een getal lezen, veranderen
  en terugschrijven als een ondeelbare stap.

- **Backpressure** - tegendruk: het mechanisme dat een producent afremt als de
  consument het niet bijhoudt. Voorkomt dat een wachtrij overloopt en data
  verloren gaat.

- **BIOS** - de oude firmware van pc's. Start bij het aanzetten en laadt de
  bootsector.

- **Bit** - het kleinste stukje informatie: een 0 of een 1.

- **Bootloader** - het eerste programma dat draait bij het opstarten. Zijn taak
  is de kernel starten.

- **Bootsector** - bij x86: het eerste blokje van 512 bytes op een schijf, dat de
  BIOS laadt. Eindigt op de handtekening `0x55 0xAA`.

- **Buddy-systeem** - een manier om geheugen te beheren door het steeds in
  tweeen te delen. Je kunt snel een blok van de juiste grootte vinden, en als
  twee naastliggende blokken vrijkomen, voeg je ze weer samen.

- **Byte** - acht bits samen. Een byte kan een getal van 0 tot en met 255 zijn.

- **Cache (CPU-cache)** - een klein, supersnel stukje geheugen in de processor
  dat kopietjes bewaart van recent gebruikte data uit het hoofdgeheugen. Maakt
  alles sneller doordat de processor niet steeds hoeft te wachten.

- **Cache-coherentie** - het probleem (en de oplossing) dat meerdere kernen elk
  hun eigen cache hebben, maar allemaal hetzelfde geheugen delen. Als kern A
  een waarde verandert, moet kern B dat te weten komen. Protocollen als MESI
  regelen dit.

- **Cache line** - het kleinste blokje dat de cache in een keer ophaalt, meestal
  64 bytes. Zelfs als je maar een byte nodig hebt, haalt de cache het hele
  blokje op.

- **Cache miss** - het moment dat de data die de processor nodig heeft *niet* in
  de cache zit. De processor moet dan wachten op het trage hoofdgeheugen.

- **CAS (Compare-and-Swap)** - een atomaire operatie: "als de waarde nog X is,
  vervang hem door Y." Lukt het niet (iemand anders was eerder), dan probeer je
  opnieuw. De basis van veel lock-free structuren.

- **Clone** - een systeemaanroep (in Linux) om een nieuw proces of een nieuwe
  thread te maken. Lijkt op fork maar geeft meer controle over wat gedeeld
  wordt.

- **Completie-wachtrij** - de ring waar de kernel (of hardware) antwoorden
  neerzet nadat een opdracht is uitgevoerd. De andere helft van een queue pair
  naast de submission queue.

- **Context** - de volledige toestand van een draaiend programma: alle
  registers, de program counter, en welke adresruimte actief is. Wat de kernel
  moet opslaan als hij van het ene naar het andere programma wisselt.

- **Context switch** - het wisselen van de ene context naar de andere: de
  processor slaat de registers van het lopende programma op, laadt de registers
  van het volgende, en gaat daar verder.

- **Cooperatief scheduling** - een manier van inplannen waarbij een programma
  zelf beslist wanneer het de processor teruggeeft. Als het dat niet doet,
  krijgen anderen nooit een beurt. Het tegenovergestelde van preemptief.

- **Copy-on-write** - een truc: als twee processen dezelfde geheugenpagina
  delen, wordt er pas een kopie gemaakt als een van beiden erin wil
  schrijven. Bespaart geheugen bij fork.

- **Coroutine** - een functie die zichzelf kan pauzeren en later weer verder
  gaan. Lichter dan een thread, want er is geen context switch door de kernel
  nodig. Zie ook: async/await, fiber, strand.

- **Cross-toolchain** - gereedschap dat op jouw computer (de host) draait, maar
  programma's maakt voor een andere processor (het target).

- **Demand paging** - een techniek waarbij geheugenpagina's pas echt worden
  toegewezen als een programma ze voor het eerst gebruikt. Tot die tijd zijn ze
  alleen "beloofd". Bespaart geheugen.

- **Device tree** - een gegevensbestand dat de firmware aan de kernel geeft en
  dat beschrijft welke hardware er in de computer zit: processoren, geheugen,
  apparaten. Veel gebruikt op ARM64 en RISC-V.

- **DMA** - Direct Memory Access. Een manier waarbij een apparaat (zoals een
  schijf of netwerkkaart) zelf direct in het geheugen kan lezen en schrijven,
  zonder dat de processor elk byte hoeft door te geven.

- **Driver** - code die met een apparaat praat (schijf, netwerk, scherm).

- **EEVDF** - Earliest Eligible Virtual Deadline First. Een scheduler-algoritme
  dat elk programma een virtuele deadline geeft en steeds het programma met de
  vroegste deadline aan de beurt laat.

- **Emulator** - een programma dat een hele computer nadoet in software. Wij
  gebruiken QEMU.

- **Endianness** - de volgorde waarin de bytes van een groot getal in het
  geheugen staan. Onze drie ISA's zijn *little-endian* (kleinste byte eerst).

- **Epoll** - een mechanisme in Linux om efficient op veel bestanden of sockets
  tegelijk te wachten. Het OS vertelt je welke klaar zijn, in plaats van dat je
  ze allemaal een voor een controleert.

- **Event loop** - een programma-structuur die in een lus wacht op gebeurtenissen
  (toetsaanslagen, netwerkdata, timers) en voor elke gebeurtenis de juiste
  handler aanroept. De kern van Node.js en de meeste servers.

- **Fenced memory** - een geheugenoperatie met een hek (fence): de processor
  garandeert dat alle eerdere lees- of schrijfacties klaar zijn voordat hij
  verder gaat. Nodig bij gelijktijdig programmeren op meerdere kernen.

- **Fiber** - een lichtgewicht uitvoerdraad die door een bibliotheek wordt
  beheerd, niet door de kernel. Vergelijkbaar met een green thread of strand.

- **Firmware** - software die vast in een chip zit en klaar is zodra de stroom
  aangaat (BIOS, UEFI, OpenSBI).

- **FMA** - Fused Multiply-Add. Een instructie die vermenigvuldigen en optellen
  in een stap doet (a * b + c). Heel belangrijk voor matrixberekeningen.

- **Fork** - een systeemaanroep die een kopie maakt van het huidige proces. Het
  kindproces krijgt dezelfde code en data, maar draait apart. Vaak gecombineerd
  met copy-on-write.

- **FPU** - Floating-Point Unit. Het onderdeel van de processor dat met
  kommagetallen rekent (zoals 3.14). Zie ook: hard float, soft float.

- **Frame (geheugenframe)** - een blok fysiek geheugen van een vaste grootte
  (meestal 4096 bytes = 4 KiB). Het OS verdeelt al het geheugen in zulke
  frames en houdt bij welke vrij zijn.

- **Frame-allocator** - het stuk code in de kernel dat bijhoudt welke
  geheugenframes vrij zijn en ze uitdeelt als een programma geheugen nodig
  heeft.

- **Gebruikersruimte (user space)** - de wereld waar gewone programma's draaien.
  Ze mogen beperkt en vragen de rest aan de kernel.

- **Gebruikers-stand (user mode)** - de beperkte stand van de processor voor
  gewone programma's.

- **Geheugen (RAM)** - een lange rij vakjes waarin de computer getallen bewaart.
  Elk vakje heeft een adres.

- **GIC** - Generic Interrupt Controller. De interrupt-controller op ARM64. Heeft
  een distributor (GICD) die interrupts verdeelt over de kernen, en een
  redistributor (GICR) per kern. Zie ook: APIC, PIC, PLIC.

- **GIL (Global Interpreter Lock)** - een slot dat ervoor zorgt dat maar een
  thread tegelijk Python-code kan uitvoeren. Beschermt tegen race conditions,
  maar voorkomt ook dat Python echt parallel draait op meerdere kernen.

- **Goroutine** - Go's versie van een lichtgewicht taak. Duizenden goroutines
  worden door de Go-runtime verdeeld over een klein aantal OS-threads
  (M:N-mapping).

- **Green thread** - een thread die door een bibliotheek of runtime wordt
  beheerd in plaats van door het OS. De kernel ziet er niets van. Vergelijkbaar
  met fiber of strand.

- **Hard float** - rekenen met kommagetallen via echte hardware-instructies van
  de FPU. Snel, maar de processor moet het ondersteunen. Tegenovergestelde van
  soft float.

- **Hex (hexadecimaal)** - een telstelsel met zestien cijfers (0-9 en a-f).
  Getallen met `0x` ervoor staan in hex, bijvoorbeeld `0x7c00`.

- **Host** - jouw eigen computer, waarop je bouwt.

- **IEEE 754** - de standaard die bepaalt hoe kommagetallen worden opgeslagen
  in de computer. Definieert formaten als float (32 bit) en double (64 bit),
  en speciale waarden als NaN (niet een getal) en oneindig.

- **Instructie** - een klein stapje dat de processor uitvoert.

- **Instructieset (ISA)** - de "taal" van een processor: welke instructies hij
  begrijpt en de regels erbij.

- **Interrupt** - een onderbreking: de hardware tikt de processor op de schouder
  als er iets is (toets, klok, netwerk).

- **Interrupt-controller** - het stukje hardware dat interrupt-signalen van
  apparaten opvangt, een nummer toekent, prioriteiten bepaalt, en het signaal
  aan de juiste processorkern doorgeeft. Voorbeelden: APIC, GIC, PLIC.

- **io_uring** - een modern I/O-mechanisme in Linux dat werkt met twee
  ringbuffers (een submission queue en een completion queue) in gedeeld
  geheugen. Vergelijkbaar met het queue pair in rheo-os.

- **IOMMU** - Input/Output Memory Management Unit. Hardware die DMA-verkeer van
  apparaten controleert: een apparaat mag alleen bij geheugen dat hem is
  toegewezen. Beschermt tegen foute of kwaadaardige hardware.

- **JIT (Just-In-Time)** - een techniek waarbij code pas op het moment dat hij
  nodig is wordt vertaald naar machinecode. Gebruikt door JavaScript-engines
  (V8, JavaScriptCore) en Java. Sneller dan een interpreter, flexibeler dan
  vooraf compileren.

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

- **Lock-free** - een manier van gelijktijdig programmeren zonder sloten
  (mutexen). Gebruikt atomaire operaties (CAS) zodat er altijd vooruitgang is,
  ook als een thread wordt onderbroken.

- **M:N-mapping** - een model waarbij M lichtgewicht taken (green threads,
  goroutines) worden verdeeld over N echte OS-threads. De runtime verdeelt het
  werk.

- **Machinecode** - instructies als kale getallen; de enige taal die de processor
  echt begrijpt.

- **Memory-mapped I/O** - hardware aansturen door naar speciale adressen te
  schrijven of van ze te lezen (bijvoorbeeld de seriele poort). Zie ook: MMIO.

- **MESI** - een protocol voor cache-coherentie. Elke cache line is in een van
  vier toestanden: Modified (veranderd), Exclusive (alleen bij mij), Shared
  (gedeeld), Invalid (ongeldig). Zo weten de kernen van elkaar wat er
  veranderd is.

- **Migratie (thread-migratie)** - het verplaatsen van een thread van de ene
  processorkern naar de andere. Kan nodig zijn voor balans, maar kost tijd
  doordat de cache "koud" is op de nieuwe kern.

- **MMIO** - Memory-Mapped I/O. Hardware aansturen door te lezen en schrijven
  naar speciale geheugenadressen. Dezelfde load/store-instructies als voor
  gewoon geheugen, maar de hardware reageert erop.

- **Multi-core** - een computer met meerdere processoren (kernen) die tegelijk
  werken.

- **Mutex** - Mutual Exclusion. Een slot dat ervoor zorgt dat maar een thread
  tegelijk een stuk code of een stuk geheugen mag gebruiken. Wie het slot niet
  heeft, moet wachten.

- **NPU** - Neural Processing Unit. Een chip gespecialiseerd in de
  matrixberekeningen van neurale netwerken. Zit in veel moderne telefoons en
  laptops. Vergelijkbaar met een TPU.

- **NUMA** - Non-Uniform Memory Access. Een computerontwerp waarbij elke
  processorkern een eigen stuk geheugen heeft dat snel is, en het geheugen van
  andere kernen langzamer kan bereiken.

- **Page fault** - een onderbreking die optreedt als een programma een
  geheugenpagina probeert te gebruiken die (nog) niet echt is toegewezen. De
  kernel kan dan de pagina alsnog laden (demand paging) of het programma
  stoppen als het fout is.

- **Paging** - het geheugen in blokjes (pagina's) verdelen en per programma
  bepalen welke blokjes het mag gebruiken.

- **Paginatabel** - de tabel die de processor gebruikt om virtuele adressen
  (wat het programma ziet) om te zetten naar fysieke adressen (waar het echt
  in het geheugen staat). Elke regel zegt: "virtueel blokje X is fysiek
  blokje Y."

- **PCIe** - Peripheral Component Interconnect Express. De snelle bus waarmee
  uitbreidingskaarten (GPU, NVMe-schijf, netwerkkaart) met de processor
  communiceren.

- **PIC** - Programmable Interrupt Controller. De oude interrupt-controller van
  x86, vervangen door de APIC. Zie ook: APIC, GIC, PLIC.

- **PIO** - Programmed I/O. Hardware aansturen door de processor speciale
  in/out-instructies te laten uitvoeren. Oud en langzaam; tegenwoordig meestal
  vervangen door MMIO of DMA.

- **PLIC** - Platform-Level Interrupt Controller. De interrupt-controller van
  RISC-V voor externe apparaten. Zie ook: APIC, GIC, PIC.

- **Pollen** - steeds zelf controleren of er iets gebeurd is (het tegenovergestelde
  van wachten op een interrupt).

- **Preemptief scheduling** - een manier van inplannen waarbij het OS een
  programma kan onderbreken, ook als dat programma niet zelf stopt. De
  timer-interrupt dwingt dit af. Tegenovergestelde van cooperatief.

- **Prioriteitsinversie** - een probleem waarbij een laag-prioriteit-thread een
  slot vasthoudt dat een hoog-prioriteit-thread nodig heeft. Het
  hoog-prioriteit-programma wordt geblokkeerd door het laag-prioriteit-programma.

- **Proces** - een draaiend programma met zijn eigen geheugen.

- **Producent-consument** - het basispatroon van een wachtrij: de producent zet
  items erin, de consument haalt ze eruit.

- **Processor (CPU)** - het onderdeel dat instructies uitvoert.

- **Program counter** - het register dat aanwijst welke instructie nu aan de beurt
  is.

- **QEMU** - de emulator die wij gebruiken om de drie processoren na te doen.

- **RCU** - Read-Copy-Update. Een techniek waarbij lezers altijd door kunnen
  zonder slot, en schrijvers een kopie maken, die aanpassen en pas dan de oude
  versie vervangen. Zeer efficient als er veel meer lezers dan schrijvers zijn.

- **Real mode** - de oude 16-bit stand waarin een x86 opstart.

- **Register** - een heel snel opslagplekje in de processor, voor een getal.

- **Ringbuffer** - een buffer die als een ring werkt: als je aan het einde
  komt, begin je weer vooraan. Gebruikt voor wachtrijen in de kernel,
  schijf-I/O en netwerk. Een ringbuffer heeft een lees-positie (kop) en een
  schrijf-positie (staart).

- **RISC-V** - een open, eenvoudige instructieset. Onze startplek in dit boek.

- **Round-robin** - een eenvoudige manier van scheduling: elk programma krijgt
  om de beurt evenveel tijd, in een vaste volgorde, als een rondje langs
  iedereen.

- **Scheduling (inplannen)** - programma's om de beurt de processor geven, heel
  snel achter elkaar, zodat ze lijken samen te draaien.

- **Semafor** - een teller die regelt hoeveel threads tegelijk bij een
  gedeelde bron mogen. Een mutex is een semafor met waarde 1.

- **Seqlock** - een slot voor data die heel vaak gelezen wordt en zelden
  geschreven. De schrijver verhoogt een teller voor en na het schrijven; de
  lezer controleert die teller om te zien of er tussenin geschreven is. Zo
  ja, dan leest hij opnieuw.

- **Seriele poort (UART)** - een simpel stukje hardware dat tekens een voor een
  verstuurt. Onze eerste manier om tekst te tonen.

- **SIMD** - Single Instruction, Multiple Data. Een type instructie dat dezelfde
  bewerking tegelijk uitvoert op meerdere getallen. Bijvoorbeeld: acht getallen
  tegelijk optellen in een stap. Voorbeelden: SSE, AVX, NEON.

- **Slab-allocatie** - een manier om geheugen te beheren voor objecten van
  dezelfde grootte. Je maakt vooraf een "plaat" (slab) met vakjes, en deelt
  die snel uit. Efficient voor veelgebruikte structuren in de kernel.

- **Soft float** - rekenen met kommagetallen in software, zonder speciale
  hardware-instructies. Langzaam maar werkt overal. Tegenovergestelde van
  hard float.

- **Spinlock** - een slot waarbij de wachtende thread in een lus draait ("spint")
  tot het slot vrijkomt. Snel voor hele korte wachttijden, maar verspilt
  processortijd bij langere wachttijden.

- **SPSC/MPSC/MPMC** - afkortingen voor het aantal producenten en consumenten
  bij een wachtrij. SPSC: een producent, een consument. MPSC: meerdere
  producenten, een consument. MPMC: meerdere van beide.

- **Stack (stapel)** - een stukje geheugen dat functies gebruiken voor tijdelijke
  gegevens. Groeit naar beneden.

- **Stack pointer** - het register dat aanwijst waar de bovenkant van de stack is
  (bij RISC-V heet dat `sp`).

- **Starvation** - uithongering: een thread of proces krijgt nooit de processor
  omdat andere steeds voorrang krijgen. Het programma werkt niet, maar is ook
  niet gecrashed.

- **Store** - een byte (of getal) naar het geheugen schrijven.

- **Syscall (systeemaanroep)** - een verzoek van een gewoon programma aan de
  kernel, bijvoorbeeld "open dit bestand".

- **Systolische array** - een raster van rekeneenheden waar data doorheen
  stroomt als water door buizen. Elke eenheid doet een vermenigvuldiging en
  een optelling. Gebruikt in TPU's en Intel AMX.

- **Target (doel)** - de processor waarvoor je een programma maakt.

- **Tegel (tile)** - een klein rechthoekig blokje data (meestal uit een matrix)
  dat als eenheid wordt verwerkt. GPU's, TPU's en NPU's werken allemaal met
  tegels om massaal parallel te rekenen.

- **Thread** - een uitvoerdraad binnen een proces. Threads delen het geheugen
  van hun proces maar hebben elk een eigen stack, registers en program counter.

- **TLB** - Translation Lookaside Buffer. Een kleine cache in de processor die
  recent gebruikte vertalingen van virtuele naar fysieke adressen onthoudt.
  Zonder TLB zou de processor bij elke geheugentogang de hele paginatabel
  moeten doorlopen.

- **TPU** - Tensor Processing Unit. Google's chip voor matrixberekeningen,
  gebaseerd op een systolische array. Vergelijkbaar met een NPU.

- **Trap** - een onderbreking die door de processor zelf wordt veroorzaakt, niet
  door externe hardware. Voorbeelden: een onbekende instructie, een deling door
  nul, of een syscall. Wordt afgehandeld via de vectortabel.

- **UEFI** - de moderne opvolger van de BIOS.

- **Vectortabel** - een tabel met adressen die de processor raadpleegt als er
  een interrupt of trap binnenkomt. Elke soort interrupt heeft een eigen
  regel in de tabel die naar de juiste handler wijst.

- **Virtueel geheugen** - een systeem waarbij elk programma denkt dat het een
  eigen, groot, aaneengesloten geheugen heeft. De paging-hardware vertaalt
  de virtuele adressen van het programma naar echte fysieke adressen in het
  RAM.

- **Wait-free** - een sterkere vorm van lock-free: elke thread maakt altijd
  vooruitgang in een vast aantal stappen, ongeacht wat andere threads doen.
  Zeer moeilijk te programmeren maar geeft de sterkste garanties.

- **Work stealing** - een techniek waarbij een processor die niets te doen heeft
  werk "steelt" uit de wachtrij van een andere processor. Houdt alle kernen
  bezig zonder centrale verdeling.

- **x86-64 (amd64)** - de instructieset van de meeste laptops en desktops. Oud en
  daardoor ingewikkeld, met een bijzondere opstartmanier.

- **Zero-copy** - een techniek waarbij data wordt gedeeld of verplaatst *zonder*
  het te kopieren. Bijvoorbeeld: twee processen kijken naar dezelfde
  geheugenpagina in plaats van de data te kopieren. Bespaart tijd en geheugen.

Terug naar de [inhoudsopgave](README.md).
