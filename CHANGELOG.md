# O que mudou

Cada versão fica ligada ao episódio em que foi feita. As ideias da comunidade levam o nome de quem as deu.

## [v0.2.0] Pedidos de administrador à distância: 2026-11-28

- **Novo:** no ecrã inteiro (`/?app=ecra`) passas a ver os pedidos de administrador do Windows (UAC), o ecrã de bloqueio e o Ctrl+Alt+Del, e podes aceitá-los ou recusá-los com o rato e o teclado. Até agora a página ficava parada, porque o Windows mostra estes ecrãs num ambiente à parte onde só a conta SYSTEM entra. É opcional: precisa de uma segunda tarefa agendada (ver o README, *Pedidos de administrador*).
- **Corrigido:** quando o `ffmpeg`/NVENC encravava sem soltar imagens, a página ficava em "A ligar ao PC…" para sempre. Agora a ligação passa sozinha para JPEG.

## [v0.1.1] Apps escondidas na bandeja: 2026-11-24

- **Corrigido:** apps que arrancam escondidas na bandeja do sistema (por exemplo apps feitas em Electron com ícone na bandeja) ficavam presas em "A abrir a app...". Agora o KCU Window encontra a janela principal escondida, mostra-a e começa a transmitir.

## [ep00] Primeira versão pública: 2026-11-24

- Uma app do Windows por página, com rato, teclado e toque.
- Vídeo H.264 com placa NVIDIA (até 60 FPS), ou JPEG sem ela.
- Ecrã inteiro (`/?app=ecra`), som da app e microfone do aparelho.
- Câmara do aparelho na app, através da OBS Virtual Camera.
- Enviar e descarregar ficheiros, com senha de uso único.
- Licença PolyForm Noncommercial: grátis para uso pessoal.
