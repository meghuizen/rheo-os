# Hoofdstuk 24 - Apparaten aansturen: drivers en hardware

Een computer zonder apparaten is een rekenmachine in een gesloten doos. De
schijf, de netwerkkaart, de grafische kaart, het toetsenbord - dat zijn de
handen en ogen van je machine. Maar elk apparaat spreekt zijn eigen taal. De
code die die taal vertaalt, heet een **driver**. In dit hoofdstuk leer je hoe
een besturingssysteem ontdekt welke apparaten er zijn, hoe het ermee praat, en
wat er komt kijken bij het schrijven van een driver.

## Apparaten ontdekken: hoe weet het OS wat er is?

Als je computer opstart, moet de kernel weten: welke schijven zitten er in,
welke netwerkkaarten, welke GPU? Die informatie komt niet vanzelf. Er zijn drie
manieren waarop de kernel het te weten komt:

**1. PCI/PCIe: de apparatenbus.** De meeste apparaten in een desktop of server
zitten op de **PCIe-bus** (Peripheral Component Interconnect Express). Die bus
werkt als een centraal register: de kernel kan elk "slot" op de bus uitlezen
en vragen: "Wie zit hier? Wat voor apparaat ben je?" Elk apparaat heeft een
**vendor ID** (fabrikant) en een **device ID** (model). Zo kan de kernel
zeggen: "Slot 3 is een AMD-videokaart", zonder dat iemand het vertelt.

Dit heet **enumeratie**: de bus aflopen, alle sloten bekijken, en voor elk
gevonden apparaat de juiste driver opstarten.

**2. Device tree: een kaart van de hardware.** Op ARM- en RISC-V-systemen
levert de firmware vaak een **device tree** (apparatenboom). Dat is een soort
plattegrond: "Op adres 0x10000000 zit een serieel apparaat van type 16550. Op
adres 0x10001000 zit een virtio-blok." De kernel leest die boom bij het
opstarten en weet dan wat waar zit.

**3. ACPI: tabellen van de firmware.** Op x86-servers gebruikt de firmware
**ACPI** (Advanced Configuration and Power Interface). Dat zijn tabellen in het
geheugen met informatie over processoren, geheugengebieden, interrupt-routering,
en soms hele apparaten. ACPI is veel ingewikkelder dan een device tree, maar het
idee is hetzelfde: de firmware vertelt de kernel wat er is.

In rheo-os staat de hardware-ontdekking in `kernel/src/hw/`. Die code bouwt
een `Inventory` - een lijst van alle gevonden apparaten, processoren,
geheugengebieden en NUMA-topologie. Kijk in `kernel/src/hw/mod.rs` voor de
structuur.

## Twee manieren om met hardware te praten

Als de kernel een apparaat heeft gevonden, moet hij er ook mee kunnen praten.
Dat kan op twee manieren.

### MMIO: Memory-Mapped I/O

Bij **MMIO** (Memory-Mapped I/O) doet het apparaat alsof het een stukje
geheugen is. De kernel schrijft naar een adres, en dat adres is niet echt RAM
maar een "deurtje" naar het apparaat. Je kent dit al uit hoofdstuk 5: de
serieel poort aansturen door naar adres `0x10000000` te schrijven.

Het mooie van MMIO: je gebruikt gewone load- en store-instructies. Er zijn geen
speciale instructies nodig. Vrijwel alle moderne apparaten werken zo.

### PIO: Port I/O

Op oudere x86-machines is er ook **PIO** (Port I/O). Hierbij heeft het
apparaat een "poort-nummer" en je gebruikt speciale instructies (`in` en `out`)
om ermee te praten. PIO is oud en langzaam, maar je komt het nog tegen bij
bijvoorbeeld de serieel poort op x86.

MMIO heeft gewonnen. Op ARM en RISC-V bestaat PIO niet eens. Vrijwel alle
nieuwe apparaten gebruiken MMIO.

```text
            PIO (oud, alleen x86)          MMIO (modern, overal)

  CPU --[in/out instructie]--> Poort   CPU --[load/store]--> Adres
                                |                              |
                            Apparaat                       Apparaat
```

## DMA: het apparaat leest zelf het geheugen

Bij MMIO en PIO stuurt de CPU elk stukje data zelf naar het apparaat. Dat is
prima voor een paar bytes, maar stel je voor dat je een heel bestand van de
schijf wilt lezen. Miljoenen bytes. Als de CPU elke byte zelf moet verplaatsen,
is hij de hele tijd bezig met sjouwen in plaats van met rekenen.

**DMA** (Direct Memory Access) lost dit op. Hierbij zeg je tegen het apparaat:
"Lees de data uit het geheugen vanaf adres X, lengte Y." Het apparaat doet dat
zelf, zonder de CPU erbij te betrekken. Als het klaar is, stuurt het een
**interrupt** (een seintje) naar de CPU.

Stel je voor dat de CPU een manager is en het apparaat een medewerker. Bij
MMIO/PIO geeft de manager elke doos zelf aan. Bij DMA zegt de manager: "Pak
zelf de dozen uit rek 5" en gaat door met ander werk.

```text
Zonder DMA:                         Met DMA:

CPU: lees byte 1 -> stuur           CPU: "Apparaat, lees zelf
CPU: lees byte 2 -> stuur                vanaf adres X, lengte Y"
CPU: lees byte 3 -> stuur           CPU: doet ander werk...
...                                 Apparaat: leest zelf het geheugen
CPU: lees byte 1000000 -> stuur     Apparaat: "Klaar!" (interrupt)
```

### IOMMU: de beveiliger van DMA

Maar hier is een probleem. Als een apparaat zelf het geheugen kan lezen en
schrijven, wat houdt het dan tegen om in het geheugen van *een ander programma*
te kijken? Of in het geheugen van de kernel?

Daar is de **IOMMU** (I/O Memory Management Unit) voor. Die werkt als een
slagboom tussen het apparaat en het geheugen. De kernel stelt de IOMMU in met
regels: "Dit apparaat mag alleen bij *deze* stukken geheugen." Probeert het
apparaat ergens anders te lezen, dan blokkeert de IOMMU het en meldt het aan
de kernel.

In rheo-os staat de IOMMU-code in `kernel/src/hw/iommu.rs` (x86-64, Intel
VT-d) en `kernel/src/hw/smmuv3.rs` (ARM64, SMMUv3). De `iommu`-testkernel
bewijst dat het werkt: een apparaat kan data lezen via een toegestaan domein,
en de IOMMU blokkeert het als het domein wordt ingetrokken.

## De levenscyclus van een driver

Een driver doorloopt altijd dezelfde stappen:

**1. Ontdekken (probe).** De kernel loopt de bus af en vindt een apparaat. Op
basis van de vendor/device ID kiest hij de juiste driver.

**2. Initialiseren (init).** De driver zet het apparaat klaar: reset het, stel
instellingen in, maak geheugen vrij voor de wachtrijen waar het apparaat mee
werkt. Pas na deze stap is het apparaat bruikbaar.

**3. Bedienen (operate).** De driver stuurt verzoeken naar het apparaat
(lezen, schrijven, een pakketje versturen) en ontvangt antwoorden. Dit is waar
het apparaat echt zijn werk doet. Vaak gebeurt dit via een wachtrij van
opdrachten (zie het volgende hoofdstuk).

**4. Opruimen (cleanup).** Als het apparaat niet meer nodig is - of als de
driver wordt gestopt - moet alles netjes worden afgesloten. DMA-gebieden
vrijgeven, interrupts uitschakelen, het apparaat stoppen.

In rheo-os kun je dit patroon terugzien in elke driver:

- `kernel/src/hw/nvme.rs` - de NVMe-schijfdriver. Een echt NVMe-apparaat wordt
  gereset, de admin-wachtrij wordt aangemaakt, de controller ingeschakeld, en
  dan kunnen er lees- en schrijfopdrachten worden verstuurd.
- `kernel/src/hw/virtio_blk.rs` - de virtio-blk driver. Feature negotiation,
  een virtqueue opzetten, en dan blokverzoeken sturen.
- `kernel/src/hw/virtio_net.rs` - de netwerkkaart. Dezelfde stappen, maar dan
  voor het versturen en ontvangen van Ethernet-frames.

## Hoe een NVMe-driver werkt: een concreet voorbeeld

NVMe (Non-Volatile Memory Express) is de standaard voor moderne SSD-schijven.
Het interessante: NVMe werkt zelf met **wachtrijen** in het geheugen van de
computer.

```text
                     geheugen (RAM)
                   +-----------------+
   CPU schrijft -> | opdracht-wachtrij (SQ)  |
                   +-----------------+
                          |
                          v   deurbel (doorbell)
                   +-----------------+
                   |  NVMe-controller |  <- het apparaat
                   +-----------------+
                          |
                          v
                   +-----------------+
   CPU leest   <- | antwoord-wachtrij (CQ)  |
                   +-----------------+
```

De CPU plaatst een opdracht in de SQ (submission queue), drukt op de "deurbel"
(een write naar een MMIO-adres), en het apparaat leest de opdracht zelf uit het
geheugen (DMA). Als het klaar is, schrijft het een antwoord in de CQ
(completion queue) en stuurt een interrupt.

Dit patroon - wachtrijen in gedeeld geheugen, deurbellen, en DMA - zie je
overal: bij NVMe, bij virtio, bij io_uring, en bij de queue-pair ABI van
rheo-os zelf. Het is zo belangrijk dat het volgende hoofdstuk er helemaal aan
gewijd is.

## Samenvatting

- De kernel ontdekt apparaten via **PCIe-enumeratie** (de bus aflopen),
  **device trees** (een kaart van de firmware), of **ACPI-tabellen**.
- **MMIO** (gewone load/store naar speciale adressen) is de moderne manier om
  met hardware te praten. **PIO** (speciale instructies) is oud en alleen op
  x86.
- **DMA** laat het apparaat zelf het geheugen lezen en schrijven, zonder de
  CPU te belasten.
- De **IOMMU** beveiligt DMA: hij beperkt welk geheugen een apparaat mag
  aanraken.
- Een driver doorloopt altijd: **ontdekken**, **initialiseren**, **bedienen**,
  **opruimen**.
- In rheo-os staat alle hardware-code in `kernel/src/hw/`, met drivers voor
  NVMe, virtio-blk, virtio-net, virtio-gpu, TPM en meer.

## Oefeningen

1. Wat is het verschil tussen MMIO en PIO? Waarom heeft MMIO gewonnen?
2. Leg in je eigen woorden uit waarom DMA nodig is. Gebruik de vergelijking
   van de manager en de medewerker, of bedenk je eigen.
3. Waarom is een IOMMU belangrijk voor de veiligheid? Wat kan er misgaan als
   een apparaat onbeperkt in het geheugen kan lezen?
4. Bekijk de NVMe-driver in `kernel/src/hw/nvme.rs`. Kun je de vier stappen
   van de driver-levenscyclus (ontdekken, initialiseren, bedienen, opruimen)
   herkennen?
5. Waarom gebruikt NVMe wachtrijen in het geheugen in plaats van directe
   MMIO-lees/schrijfoperaties voor elke byte?
