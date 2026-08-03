# Hoofdstuk 12 - Virtueel geheugen: elk programma denkt dat het alles heeft

In hoofdstuk 1 zeiden we: het OS zorgt dat programma's niet in elkaars geheugen
kunnen komen. Maar hoe werkt dat precies? Het antwoord heet **virtueel geheugen**,
en het is een van de slimste trucjes in de hele computer.

## Het probleem

Stel je hebt twee programma's. Allebei willen ze geheugen gebruiken. Allebei
schrijven ze naar adres 1000. Zonder bescherming overschrijft het ene programma
de data van het andere. Chaos.

Je zou kunnen zeggen: "geef programma A adressen 0 tot 999, en programma B
adressen 1000 tot 1999." Maar dan moet elk programma precies weten welke
adressen het mag gebruiken. En als programma A meer geheugen nodig heeft? Dan zit
B in de weg.

## De truc: elk programma krijgt zijn eigen nep-adresruimte

De oplossing: elk programma denkt dat het *alle* adressen voor zichzelf heeft. Het
programma schrijft naar adres 1000, maar de processor vertaalt dat stiekem naar
een andere plek in het echte geheugen. Programma B schrijft ook naar "adres 1000",
maar dat wordt vertaald naar weer een andere echte plek.

Dat "nep"-adres heet een **virtueel adres**. De echte plek in het geheugen heet
een **fysiek adres**. De vertaling gebeurt in de hardware, door een onderdeel dat
de **MMU** heet (Memory Management Unit). De MMU zit in de processor zelf.

```text
Programma A ziet:             Echt geheugen (fysiek):
+-------------------+         +-------------------+
| 0x0000  code      |  -----> | 0x5000  A's code  |
| 0x1000  data      |  -----> | 0x8000  A's data  |
+-------------------+         +-------------------+

Programma B ziet:
+-------------------+
| 0x0000  code      |  -----> | 0xA000  B's code  |
| 0x1000  data      |  -----> | 0xD000  B's data  |
+-------------------+         +-------------------+
```

Beide programma's denken dat ze op adres `0x1000` schrijven. Maar de MMU stuurt
A naar fysiek `0x8000` en B naar fysiek `0xD000`. Ze kunnen elkaar niet raken,
zelfs als ze hetzelfde virtuele adres gebruiken.

## Pagina's: het geheugen in blokjes

De vertaling werkt niet per byte (dat zou een gigantische vertaaltabel nodig
hebben). In plaats daarvan wordt het geheugen opgedeeld in **pagina's**: blokjes
van een vaste grootte, meestal **4 KiB** (4096 bytes). Elk blokje heeft een
nummer.

- Een virtueel blokje heet een **pagina** (page).
- Een fysiek blokje heet een **frame** (of *page frame*).

De vertaling is dus: "virtuele pagina 5 -> fysiek frame 12." Alle 4096 bytes
in dat blokje worden als geheel vertaald.

```text
Virtuele adressen          Fysiek geheugen
van programma A:           (het echte RAM):

pagina 0 ------+           +---------+
pagina 1 ---+  +---------->| frame 3 |
pagina 2    |              +---------+
  ...       +------------->| frame 7 |
                           +---------+
                           | frame 8 |  (van B)
                           +---------+
```

## Paginatabellen: het telefoonboek

De MMU moet voor elke pagina weten: naar welk frame verwijst hij? Die informatie
staat in een **paginatabel** (page table). Dat is een datastructuur in het
geheugen die de processor leest.

Maar er is een probleem: met 64-bit adressen zijn er *enorm* veel mogelijke
pagina's. Een platte tabel (een rij met een ingang per pagina) zou veel te groot
zijn. Daarom gebruiken processoren een **boom van meerdere niveaus**.

Op RISC-V met Sv39 (39-bit adressen) zijn er drie niveaus:

```text
Virtueel adres (39 bit):
+----------+---------+---------+-------------+
| VPN[2]   | VPN[1]  | VPN[0]  | offset (12) |
| (9 bit)  | (9 bit) | (9 bit) |             |
+----------+---------+---------+-------------+
     |           |         |
     v           v         v
  Niveau 2    Niveau 1   Niveau 0    -> fysiek frame
  tabel       tabel      tabel         + offset
  (512        (512       (512
  ingangen)   ingangen)  ingangen)
```

De processor doorloopt de boom van boven naar beneden. In elke tabel zoekt hij de
ingang die past bij dat stukje van het adres. Aan het einde vindt hij het fysieke
framenummer. Het **offset** (de laatste 12 bits) vertelt welke byte *binnen* die
pagina het is.

Op x86-64 zijn er vier niveaus (PML4 -> PDPT -> PD -> PT). Op ARM64 met 4 KiB
pagina's ook vier. Het idee is overal hetzelfde: een boom die je van boven
naar beneden doorloopt.

## De TLB: een snelkopie

Die wandeling door drie of vier tabellen kost tijd. Elke keer dat de processor
een geheugenadres gebruikt, moet hij de boom doorlopen? Dat zou veel te langzaam
zijn.

Daarom heeft de processor een **TLB** (Translation Lookaside Buffer): een klein,
heel snel geheugenhoekje waar de meest recente vertalingen worden onthouden.

Vergelijk het met een telefoon: je zoekt niet elke keer het hele telefoonboek
door. De nummers die je vaak belt, sla je op in je recente gesprekken. De TLB is
dat lijstje met recente gesprekken.

Als de vertaling in de TLB zit: klaar, geen tabelwandeling nodig. Als hij er niet
in zit (**TLB miss**): dan moet de processor alsnog de boom doorlopen, en slaat
het resultaat op in de TLB voor de volgende keer. We komen in hoofdstuk 14 terug
op wat er met de TLB gebeurt als de processor van programma wisselt.

## Bescherming: het echte doel

Virtueel geheugen is niet alleen handig; het is de kern van de beveiliging.
Elke ingang in de paginatabel heeft **vlaggen** die zeggen wat er mag:

- **Geldig** (valid): is deze pagina in gebruik?
- **Leesbaar** (readable): mag de code hieruit lezen?
- **Schrijfbaar** (writable): mag de code hiernaar schrijven?
- **Uitvoerbaar** (executable): mag de processor hier instructies uit lezen?
- **Gebruiker** (user): mag code in gebruikers-stand hierbij?

Als een programma iets probeert dat niet mag - schrijven naar een pagina die
alleen leesbaar is, of een adres gebruiken dat niet geldig is - dan genereert de
MMU een **page fault** (paginafout). Dat is een soort interrupt: de processor
stopt het programma en springt naar de kernel. De kernel beslist dan wat er
gebeurt: het programma stoppen, of het probleem oplossen.

## Page faults: niet altijd fout

Niet elke page fault is een echte fout. De kernel gebruikt page faults ook als
slim gereedschap:

- **Lazy allocation** (lui toewijzen): de kernel belooft geheugen aan een
  programma, maar wijst pas echt een fysiek frame toe als het programma het
  voor het eerst gebruikt. De eerste keer is er een page fault, de kernel wijst
  dan snel een frame toe en laat het programma verder gaan. Zo verspil je geen
  geheugen aan pagina's die nooit worden aangeraakt.

- **Demand paging** (op verzoek laden): bij het starten van een programma hoeft
  de kernel niet het hele programma in het geheugen te laden. Pas als het
  programma een pagina probeert te lezen die er nog niet is, laadt de kernel die
  van schijf. Dat maakt het opstarten sneller.

- **Copy-on-write** (kopieer bij schrijven, afgekort COW): als een programma
  een kopie van zichzelf maakt (dat heet **fork**), deelt de kernel in het begin
  de geheugen-frames. Pas als een van de twee *schrijft*, maakt de kernel een
  echte kopie van alleen die ene pagina. Zo bespaar je enorm veel geheugen en
  tijd.

```text
Page fault: de slimme truc

Programma raakt adres 0x3000 aan
           |
           v
MMU: "die pagina is niet geldig!"
           |
           v
Page fault -> kernel krijgt de controle
           |
           v
Kernel: "ah, lazy allocation - ik wijs een frame toe"
           |
           v
Kernel past de paginatabel aan, frame is nu geldig
           |
           v
Programma gaat verder, merkt niks
```

## Virtueel geheugen in rheo-os

In rheo-os is paging volledig gebouwd voor alle drie de ISA's:

- De paginatabellen per ISA staan in `kernel/src/arch/<isa>/paging.rs`. RISC-V
  gebruikt Sv39 (drie niveaus), ARM64 een 4 KiB-granule (vier niveaus), en
  x86-64 vier niveaus (PML4).
- Het frame-beheer (welke fysieke frames zijn vrij of bezet) zit in
  `kernel/src/mm/frames.rs` - een bitmap-allocator.
- De kernel draait in de **bovenste helft** van de adresruimte, zodat de
  onderste helft helemaal vrij is voor programma's (de "higher half" indeling
  uit `docs/MEMORY.md`).
- Page faults worden afgehandeld in `kernel/src/user.rs` (`on_user_trap`), en
  voor Linux-programma's in `kernel/src/linux/mod.rs` waar demand paging en
  copy-on-write echt werken.

## Samenvatting

- **Virtueel geheugen** geeft elk programma zijn eigen "nep"-adresruimte.
  De **MMU** in de processor vertaalt virtuele adressen naar fysieke adressen.
- Het geheugen is verdeeld in **pagina's** (4 KiB). De vertaling staat in een
  **paginatabel**, een boom met meerdere niveaus.
- De **TLB** is een snelle cache van recente vertalingen, zodat de processor
  niet elke keer de hele boom hoeft te doorlopen.
- Vlaggen in de paginatabel beschermen het geheugen: lezen, schrijven,
  uitvoeren, gebruiker/kernel.
- Een **page fault** is niet altijd een fout: de kernel gebruikt het voor lazy
  allocation, demand paging en copy-on-write.
- In rheo-os staan de paginatabellen in `kernel/src/arch/<isa>/paging.rs` en
  het frame-beheer in `kernel/src/mm/frames.rs`.

## Oefeningen

1. Leg uit waarom twee programma's allebei virtueel adres `0x1000` kunnen
   gebruiken zonder elkaars data te raken.
2. Een pagina is 4 KiB = 4096 bytes. Als je 16 KiB geheugen nodig hebt,
   hoeveel pagina's is dat?
3. Wat gebeurt er als een programma schrijft naar een adres waarvoor de
   "schrijfbaar"-vlag niet aan staat?
4. Waarom is copy-on-write (COW) sneller dan meteen al het geheugen kopieren
   bij een fork?
5. De TLB kan maar een beperkt aantal vertalingen onthouden. Bedenk waarom het
   een probleem kan zijn als de processor heel veel verschillende pagina's
   aanraakt in korte tijd.

Door naar [hoofdstuk 13](13-systeemaanroepen.md): hoe een gewoon programma de
kernel om hulp vraagt.
