# Glossário pt-BR (Vigilant Vole)

Português do Brasil. Tratamento: "você", com imperativo natural ("Digite um nome") ou frases impessoais; nunca "tu" nem "o senhor".
Aspas: “…” (e ‘…’ dentro). Maiúscula só no início da frase (sentence case). "e-mail" com hífen, "arquivo", "tela", "usuário".

| English | Português (Brasil) | Note |
|---|---|---|
| monitor (noun) | monitor (pl. monitores) | Kuma usa "Monitoramento"; "monitor" é mais curto para botões |
| to monitor | monitorar | |
| heartbeat / check | verificação (pl. verificações) | Kuma deixa "heartbeat"; aqui sempre "verificação" |
| check now | Verificar agora | |
| uptime | disponibilidade | Kuma: "Tempo de atividade"; mais curto |
| downtime | indisponibilidade / tempo fora do ar | |
| response time | tempo de resposta | |
| up | No ar | status |
| down | Fora do ar | status |
| pending | Pendente | |
| maintenance | Manutenção | |
| paused | Pausado | |
| unknown | Desconhecido | |
| incident | incidente | |
| outage | queda | |
| maintenance window | janela de manutenção | |
| status page | página de status | |
| notification channel | canal de notificação | |
| notifications | notificações | |
| alert | alerta | |
| escalation | escalonamento (em rótulos curtos: escalada) | |
| flapping | oscilando / oscilação | monitor que alterna entre no ar e fora do ar |
| retry / retries | nova tentativa / novas tentativas | igual ao Kuma |
| push monitor | monitor push | "Push" fica em inglês |
| push URL | URL de push | |
| remote agent | agente remoto | |
| group | grupo | |
| tag | tag | igual ao Kuma |
| badge | selo | |
| API token | token de API | |
| two-factor authentication | autenticação em dois fatores | |
| recovery code | código de recuperação | |
| authenticator app | app autenticador | |
| backup | backup | |
| restore | restaurar / restauração | |
| dashboard | painel | |
| settings | configurações | |
| user | usuário | |
| username | nome de usuário | |
| role | função | |
| admin / editor / viewer | Administrador / Editor / Leitor | "Admin" nos botões curtos |
| certificate | certificado | |
| expiry / expires | validade / expira | |
| retention | retenção (manter histórico por) | |
| interval | intervalo | |
| timeout | tempo limite | igual ao Kuma |
| target | destino | |
| keyword | palavra-chave | igual ao Kuma |
| webhook | webhook | |
| Apprise | Apprise | nome de produto |
| discovery | descoberta | |
| import / export | importar / exportar | |
| config as code | configuração como código | |
| session | sessão | |
| language | idioma | |
| time zone | fuso horário | |
| log in / log out | Entrar / Sair | |
| password | senha | |
| request / response | requisição / resposta | |
| header / body | cabeçalho / corpo | |
| redirect | redirecionamento | |
| secret | segredo | |
| draft / published | rascunho / publicado | |
| section | seção | |
| severity | gravidade | |
| degraded | Degradado | |
| operational | Operacional | |
| ago / in | há / em | |
| just now | agora mesmo | |
| slug | slug | |
| owner | proprietário | |
| read only / read and write | somente leitura / leitura e gravação | |
| check-in (push) | enviar sinal | não "se apresentar" |
| powered by | Com tecnologia | |
| remote browser | navegador remoto | |
| real browser | navegador real | monitor que carrega a página em um Chromium |
| headless | headless | termo técnico, não traduzido |
| screenshot | captura de tela | |
| game server | servidor de jogo | |
| server reflection | reflexão do servidor | gRPC |
| metadata | metadados | gRPC |
| privacy password | senha de privacidade | SNMPv3 |
| security level | nível de segurança | SNMPv3 |
| engine ID | engine ID | SNMPv3, não traduzido |
| unlock | Desbloquear | botão da página protegida |
| password-protected page | página protegida por senha | |
| page views | visualizações de página | |
| unique visitors | visitantes únicos | |
| referrer | referenciador | |
| Tailscale peer | peer do Tailscale | "peer" não traduzido |
| MOTD | MOTD | não traduzido |
| replica set | replicaSet | termo do MongoDB, em código |

## Estilo
- Plurais: `.one` para 0 e 1 ("{n} monitor"), `.many` para múltiplos exatos de 1.000.000 ("{n} de monitores"), `.other` nos demais ("{n} monitores").
- Datas: "{day} de {month}", meses abreviados com ponto ("jan.", "fev."), dias da semana "seg.", "ter."…
- Textos curtos e calmos, sem exclamações; botões e abas curtos.
