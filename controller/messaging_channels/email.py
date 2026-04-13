# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Email messaging channel adapter.

Supports:
- IMAP polling for incoming messages (every 30s)
- SMTP sending with threading via In-Reply-To headers
- Stripping quoted replies from message bodies

Configuration:
- MESSAGING_EMAIL_IMAP_HOST/PORT/USER/PASS
- MESSAGING_EMAIL_SMTP_HOST/PORT
"""

import asyncio
import email
import imaplib
import logging
import smtplib
import time
from email.mime.text import MIMEText
from email.mime.multipart import MIMEMultipart
from email.utils import parseaddr, formataddr
import os

log = logging.getLogger("ozma.messaging.email")

class EmailChannel:
    def __init__(self):
        self.imap_host = os.environ.get("MESSAGING_EMAIL_IMAP_HOST")
        self.imap_port = int(os.environ.get("MESSAGING_EMAIL_IMAP_PORT", "993"))
        self.imap_user = os.environ.get("MESSAGING_EMAIL_IMAP_USER")
        self.imap_pass = os.environ.get("MESSAGING_EMAIL_IMAP_PASS")
        
        self.smtp_host = os.environ.get("MESSAGING_EMAIL_SMTP_HOST")
        self.smtp_port = int(os.environ.get("MESSAGING_EMAIL_SMTP_PORT", "587"))
        self.smtp_user = os.environ.get("MESSAGING_EMAIL_SMTP_USER", self.imap_user)
        self.smtp_pass = os.environ.get("MESSAGING_EMAIL_SMTP_PASS", self.imap_pass)
        
        self.running = False
        self._on_message_callback = None

    async def start(self, on_message_callback):
        """Start the email polling loop."""
        self._on_message_callback = on_message_callback
        self.running = True
        asyncio.create_task(self._poll_loop(), name="email-poll")

    async def stop(self):
        """Stop the email polling loop."""
        self.running = False

    async def _poll_loop(self):
        """Main polling loop that checks for new emails every 30 seconds."""
        while self.running:
            try:
                await self._poll_inbox()
            except Exception as e:
                log.error("Email polling error: %s", e)
            await asyncio.sleep(30)

    async def _poll_inbox(self):
        """Poll the inbox for new messages."""
        loop = asyncio.get_event_loop()
        
        def _fetch_emails():
            mail = imaplib.IMAP4_SSL(self.imap_host, self.imap_port)
            try:
                mail.login(self.imap_user, self.imap_pass)
                mail.select('inbox')
                
                # Search for unread messages
                status, messages = mail.search(None, 'UNSEEN')
                if status != 'OK':
                    return []
                
                email_ids = messages[0].split()
                emails = []
                
                for email_id in email_ids:
                    # Fetch the email
                    status, msg_data = mail.fetch(email_id, '(RFC822)')
                    if status != 'OK':
                        continue
                    
                    # Parse the email
                    raw_email = msg_data[0][1]
                    email_message = email.message_from_bytes(raw_email)
                    
                    # Extract relevant information
                    from_addr = parseaddr(email_message['From'])[1]
                    subject = email_message['Subject'] or ""
                    message_id = email_message['Message-ID'] or ""
                    
                    # Get body content
                    body = ""
                    if email_message.is_multipart():
                        for part in email_message.walk():
                            if part.get_content_type() == "text/plain":
                                body = part.get_payload(decode=True).decode('utf-8', errors='ignore')
                                break
                    else:
                        body = email_message.get_payload(decode=True).decode('utf-8', errors='ignore')
                    
                    # Strip quoted replies (lines starting with >)
                    lines = body.split('\n')
                    clean_lines = [line for line in lines if not line.strip().startswith('>')]
                    clean_body = '\n'.join(clean_lines).strip()
                    
                    emails.append({
                        'id': email_id.decode(),
                        'from': from_addr,
                        'subject': subject,
                        'body': clean_body,
                        'message_id': message_id,
                    })
                
                return emails
            finally:
                mail.logout()
        
        emails = await loop.run_in_executor(None, _fetch_emails)
        
        for email_data in emails:
            if self._on_message_callback:
                await self._on_message_callback(
                    channel="email",
                    user_id=email_data['from'],
                    message=email_data['body'],
                    thread_id=email_data['message_id'],
                    metadata={
                        'subject': email_data['subject'],
                        'from': email_data['from']
                    }
                )

    async def send_message(self, to: str, subject: str, body: str, thread_id: str = None):
        """Send an email message."""
        loop = asyncio.get_event_loop()
        
        def _send_email():
            msg = MIMEMultipart()
            msg['From'] = self.smtp_user
            msg['To'] = to
            msg['Subject'] = subject
            
            if thread_id:
                msg['In-Reply-To'] = thread_id
                msg['References'] = thread_id
            
            msg.attach(MIMEText(body, 'plain'))
            
            server = smtplib.SMTP(self.smtp_host, self.smtp_port)
            try:
                server.starttls()
                server.login(self.smtp_user, self.smtp_pass)
                server.send_message(msg)
            finally:
                server.quit()
        
        await loop.run_in_executor(None, _send_email)
