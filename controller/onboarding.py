# SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
"""
Onboarding task system for device onboarding workflows.

Lightweight task system for tracking and managing device onboarding.
"""

import asyncio
import json
import logging
import uuid
from dataclasses import dataclass, asdict
from datetime import datetime
from pathlib import Path
from typing import Dict, List, Optional, Literal

from state import AppState

log = logging.getLogger("ozma.onboarding")

OnboardingTaskType = Literal['network_device', 'node', 'agent', 'certificate']
OnboardingTaskStatus = Literal['pending', 'in_progress', 'completed', 'failed', 'dismissed']
OnboardingTaskSource = Literal['auto_discovery', 'manual', 'node_registration']

@dataclass
class OnboardingTask:
    """A task representing a device/node that needs to be onboarded."""
    id: str
    type: OnboardingTaskType
    title: str
    description: str
    status: OnboardingTaskStatus
    source: OnboardingTaskSource
    data: dict
    created_at: datetime
    updated_at: datetime

    def to_dict(self) -> dict:
        """Convert task to dictionary for JSON serialization."""
        result = asdict(self)
        result['created_at'] = self.created_at.isoformat()
        result['updated_at'] = self.updated_at.isoformat()
        return result

    @classmethod
    def from_dict(cls, data: dict) -> 'OnboardingTask':
        """Create task from dictionary."""
        data = data.copy()
        data['created_at'] = datetime.fromisoformat(data['created_at'])
        data['updated_at'] = datetime.fromisoformat(data['updated_at'])
        return cls(**data)

class OnboardingManager:
    """Manager for onboarding tasks."""
    
    def __init__(self, state: AppState):
        self.state = state
        self.tasks: Dict[str, OnboardingTask] = {}
        self.data_dir = Path(state.data_dir) if hasattr(state, 'data_dir') else Path('/var/lib/ozma')
        self.tasks_file = self.data_dir / 'onboarding_tasks.json'
        self._lock = asyncio.Lock()
        
    async def start(self) -> None:
        """Start the onboarding manager and load existing tasks."""
        await self._load_tasks()
        # Subscribe to discovery events
        if hasattr(self.state, 'events'):
            asyncio.create_task(self._event_handler(), name="onboarding-event-handler")
        
    async def _event_handler(self) -> None:
        """Handle state events for device discovery."""
        while True:
            try:
                event = await self.state.events.get()
                if event.get('type') == 'discovery.device_found':
                    device = event.get('device')
                    if device:
                        await self.on_device_discovered(device)
            except Exception as e:
                log.warning("Error handling discovery event: %s", e)
                
    async def on_device_discovered(self, device: dict) -> Optional[OnboardingTask]:
        """Create onboarding task for discovered device if not already configured."""
        async with self._lock:
            # Check if we already have a task for this device
            device_id = device.get('id') or device.get('mac') or device.get('ip')
            if not device_id:
                return None
                
            # Skip if task already exists for this device
            for task in self.tasks.values():
                if task.data.get('device_id') == device_id:
                    return None
            
            # Create new task
            task_id = str(uuid.uuid4())
            task = OnboardingTask(
                id=task_id,
                type='network_device',
                title=f"New device discovered: {device.get('name', 'Unknown')}",
                description=f"Discovered device {device.get('name', '')} at {device.get('ip', 'N/A')}",
                status='pending',
                source='auto_discovery',
                data={
                    'device_id': device_id,
                    'device': device
                },
                created_at=datetime.now(),
                updated_at=datetime.now()
            )
            
            self.tasks[task_id] = task
            await self._save_tasks()
            
            # Emit event
            if hasattr(self.state, 'events'):
                await self.state.events.put({
                    'type': 'onboarding_task_created',
                    'task': task.to_dict()
                })
                
            return task
            
    async def complete_task(self, task_id: str, result: dict) -> bool:
        """Mark a task as completed."""
        async with self._lock:
            task = self.tasks.get(task_id)
            if not task:
                return False
                
            task.status = 'completed'
            task.updated_at = datetime.now()
            task.data['result'] = result
            await self._save_tasks()
            
            # Emit event
            if hasattr(self.state, 'events'):
                await self.state.events.put({
                    'type': 'onboarding_task_updated',
                    'task': task.to_dict()
                })
                
            return True
            
    async def dismiss_task(self, task_id: str) -> bool:
        """Dismiss a task."""
        async with self._lock:
            task = self.tasks.get(task_id)
            if not task:
                return False
                
            task.status = 'dismissed'
            task.updated_at = datetime.now()
            await self._save_tasks()
            
            # Emit event
            if hasattr(self.state, 'events'):
                await self.state.events.put({
                    'type': 'onboarding_task_updated',
                    'task': task.to_dict()
                })
                
            return True
            
    def get_tasks(self, status: Optional[OnboardingTaskStatus] = None) -> List[OnboardingTask]:
        """Get all tasks, optionally filtered by status."""
        if status:
            return [task for task in self.tasks.values() if task.status == status]
        return list(self.tasks.values())
        
    async def _load_tasks(self) -> None:
        """Load tasks from persistent storage."""
        try:
            if self.tasks_file.exists():
                data = json.loads(self.tasks_file.read_text())
                self.tasks = {
                    task_id: OnboardingTask.from_dict(task_data)
                    for task_id, task_data in data.items()
                }
                log.info("Loaded %d onboarding tasks", len(self.tasks))
        except Exception as e:
            log.warning("Failed to load onboarding tasks: %s", e)
            
    async def _save_tasks(self) -> None:
        """Save tasks to persistent storage."""
        try:
            # Ensure directory exists
            self.tasks_file.parent.mkdir(parents=True, exist_ok=True)
            
            # Convert tasks to dict
            data = {
                task_id: task.to_dict()
                for task_id, task in self.tasks.items()
            }
            
            # Write to file
            self.tasks_file.write_text(json.dumps(data, indent=2))
        except Exception as e:
            log.warning("Failed to save onboarding tasks: %s", e)
