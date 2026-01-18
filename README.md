## q3-stats-bot

quake3 match stats reporter bot for telegram made for fun. 
Bot monitors a specified directory for XML game logs and sends formatted match reports to a specified telegram chat.


To enable XML logging on the q3 server:
```code
set log_pergame "0"
set log_stat "1"
set log_xmlstats "xmlstats"
set log_default "0" // 0 - OSP, 1 - id format
```
#### You need

- Telegram Bot Token
- Telegram Chat ID (bot should be member of this chat)

1. **Configure the Token**:
   Create a `.env` file in the current directory:
   ```env
   TELOXIDE_TOKEN=your_telegram_bot_token_here
   ```
   Or you can set `TELOXIDE_TOKEN` as a shell environment variable

#### Usage
```bash
.q3-stats-bot --folder-path "/path/to/quakeserver/xmlstats" --chat-id "-100227937281"
```
